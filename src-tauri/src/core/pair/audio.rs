//! 语音消息的录音（§44 / §45 / R16）。
//!
//! 用 `cpal` 录、用 `hound` 写 16-bit PCM WAV：按设备原生采样率录，立体声降混成单声道，
//! **不做朴素重采样**（那会有混叠），所以 WAV 的采样率不固定，播放交给 `<audio>`。
//!
//! `cpal::Stream` 不是 `Send`（WASAPI 的设备对象要留在创建它的线程上），因此录音整体跑在
//! 一个专用线程里：命令层只拿得到「共享缓冲 + 停止标志 + 线程句柄」，句柄在线程结束后把
//! 录好的样本交回来，`cpal` 的类型一刻也不会离开那个线程。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SupportedStreamConfig};

/// 单条语音最长 60 秒（§45），到点自动收尾（约 5.8 MB，见 R16）
pub const MAX_RECORDING_SECS: u64 = 60;
/// 轻点一下不算语音（§45）；短于这个时长的直接丢掉
pub const MIN_RECORDING_MS: u64 = 300;
/// 16-bit 单声道 WAV 的固定头部长度（R16）
const WAV_HEADER_BYTES: u64 = 44;
/// 录音线程的轮询间隔：只用来判断「该停了」与「到 60 秒了」
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// 录完的音频：单声道 f32 样本 + 设备原生采样率
#[derive(Debug, Clone)]
pub struct RecordedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    /// 到 60 秒上限被截断了
    pub truncated: bool,
}

impl RecordedAudio {
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 {
            return 0;
        }

        self.samples.len() as u64 * 1_000 / self.sample_rate as u64
    }

    /// §45：轻点（< 300ms）不发送，避免误触
    pub fn is_too_short(&self) -> bool {
        self.duration_ms() < MIN_RECORDING_MS
    }
}

/// 这段音频写成 16-bit 单声道 WAV 之后会有多少字节。
///
/// 用来在**落盘之前**判断有没有超过附件上限（§42）：等到写完再发现太大，
/// 就只能在 `tmp/` 里留一个没人认领的 wav。
pub fn wav_size(audio: &RecordedAudio) -> u64 {
    WAV_HEADER_BYTES + audio.samples.len() as u64 * 2
}

/// 在附件上限 `max_size` 以内，这个采样率的语音最多能录多少秒（只用于错误提示）
pub fn wav_limit_secs(max_size: u64, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return 0;
    }

    max_size.saturating_sub(WAV_HEADER_BYTES) / 2 / sample_rate as u64
}

/// 把 16-bit PCM WAV 写到 `path`（R16：mono、设备原生采样率、不做重采样）
pub fn write_wav(path: &Path, audio: &RecordedAudio) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: audio.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|error| format!("创建录音文件失败: {error}"))?;

    for sample in &audio.samples {
        // f32 归一化到 i16：先夹住范围，避免超出 1.0 的样本绕回成噪音
        let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;

        writer
            .write_sample(value)
            .map_err(|error| format!("写入录音失败: {error}"))?;
    }

    writer
        .finalize()
        .map_err(|error| format!("保存录音失败: {error}"))
}

/// 把一次回调拿到的样本降混成单声道塞进缓冲，最多收 `limit` 个样本。
///
/// 用 `try_lock`：回调跑在音频线程上，绝不能被别处的锁拖住（拿不到锁就丢这一批，
/// 宁可少几个样本也不要产生杂音）。
fn push_samples<T>(input: &[T], channels: usize, buffer: &Mutex<Vec<f32>>, limit: usize)
where
    T: Sample,
    f32: FromSample<T>,
{
    let Ok(mut samples) = buffer.try_lock() else {
        return;
    };

    let channels = channels.max(1);

    for frame in input.chunks(channels) {
        if samples.len() >= limit {
            break;
        }

        let sum: f32 = frame.iter().map(|sample| f32::from_sample(*sample)).sum();

        samples.push(sum / frame.len() as f32);
    }
}

/// 正在进行的录音
pub struct Recording {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<RecordedAudio, String>>,
}

impl Recording {
    fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// 让录音线程收尾并取回结果（最多等一个轮询间隔）
    pub fn finish(self) -> Result<RecordedAudio, String> {
        self.request_stop();

        match self.handle.join() {
            Ok(result) => result,
            Err(_) => Err("录音线程异常结束".to_string()),
        }
    }
}

/// 命令层持有的录音状态
#[derive(Default)]
pub struct Recorder {
    current: Mutex<Option<Recording>>,
}

impl Recorder {
    /// 开始录音，返回设备原生采样率；已经在录时报错
    pub fn start(&self) -> Result<u32, String> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| "录音状态不可用".to_string())?;

        drop_finished(&mut current);

        if current.is_some() {
            return Err("已经在录音了".to_string());
        }

        let (recording, sample_rate) = spawn_recording()?;

        *current = Some(recording);

        Ok(sample_rate)
    }

    /// 取走当前录音（没在录就是 `None`），调用方负责 `finish` / 丢弃
    pub fn take(&self) -> Option<Recording> {
        let Ok(mut current) = self.current.lock() else {
            return None;
        };

        let recording = current.take();

        if let Some(recording) = recording.as_ref() {
            recording.request_stop();
        }

        recording
    }
}

/// 把已经自己结束的录音从槽里收掉。
///
/// 上一次录音会在两种情况下自己结束：按住不放到了 60 秒上限，或者录音线程出错。
/// 句柄不收掉的话，下一次按住说话会被「已经在录音了」挡住，用户只能重启应用。
fn drop_finished(current: &mut Option<Recording>) {
    if current
        .as_ref()
        .is_some_and(|recording| recording.handle.is_finished())
    {
        current.take();
    }
}

/// 起一个录音线程，并等它报告「录起来了」或失败原因
fn spawn_recording() -> Result<(Recording, u32), String> {
    let buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32, String>>();

    let thread_buffer = Arc::clone(&buffer);
    let thread_stop = Arc::clone(&stop);

    let handle = std::thread::Builder::new()
        .name("bongo-pair-recording".to_string())
        .spawn(move || record_until_stopped(thread_buffer, thread_stop, ready_tx))
        .map_err(|error| format!("启动录音线程失败: {error}"))?;

    // 等线程把设备打开：麦克风缺失、被独占这类问题都要在这里报给用户，
    // 不能等松开按键才发现「什么都没录到」
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(sample_rate)) => Ok((Recording { stop, handle }, sample_rate)),
        Ok(Err(error)) => {
            let _ = handle.join();

            Err(error)
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();

            Err("麦克风没有响应".to_string())
        }
    }
}

fn record_until_stopped(
    buffer: Arc<Mutex<Vec<f32>>>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<Result<u32, String>>,
) -> Result<RecordedAudio, String> {
    let (stream, sample_rate) = match open_input(&buffer) {
        Ok(value) => value,
        Err(error) => {
            let _ = ready.send(Err(error.clone()));

            return Err(error);
        }
    };

    let _ = ready.send(Ok(sample_rate));

    let started = Instant::now();
    let limit = Duration::from_secs(MAX_RECORDING_SECS);
    let mut truncated = false;

    while !stop.load(Ordering::Relaxed) {
        if started.elapsed() >= limit {
            truncated = true;

            break;
        }

        std::thread::sleep(POLL_INTERVAL);
    }

    // 先停掉流再取数据：回调还在写的话取出来的样本数会飘
    drop(stream);

    let samples = buffer
        .lock()
        .map(|samples| samples.clone())
        .unwrap_or_default();

    Ok(RecordedAudio {
        samples,
        sample_rate,
        truncated,
    })
}

/// 打开默认输入设备并起流
fn open_input(buffer: &Arc<Mutex<Vec<f32>>>) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| "找不到可用的麦克风".to_string())?;
    let supported = device
        .default_input_config()
        .map_err(|error| format!("读取麦克风配置失败: {error}"))?;
    let sample_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let limit = sample_rate as usize * MAX_RECORDING_SECS as usize;

    let stream = build_stream(&device, supported, buffer, channels, limit)?;

    stream
        .play()
        .map_err(|error| format!("启动麦克风失败: {error}"))?;

    Ok((stream, sample_rate))
}

fn build_stream(
    device: &cpal::Device,
    supported: SupportedStreamConfig,
    buffer: &Arc<Mutex<Vec<f32>>>,
    channels: usize,
    limit: usize,
) -> Result<cpal::Stream, String> {
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let on_error = |error: cpal::Error| {
        tauri_plugin_log::log::warn!("录音流出错: {error}");
    };

    macro_rules! build {
        ($sample:ty) => {{
            let buffer = Arc::clone(buffer);

            device.build_input_stream(
                config,
                move |data: &[$sample], _: &cpal::InputCallbackInfo| {
                    push_samples::<$sample>(data, channels, &buffer, limit);
                },
                on_error,
                None,
            )
        }};
    }

    let stream = match format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U32 => build!(u32),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        other => return Err(format!("不支持的采样格式：{other}")),
    }
    .map_err(|error| format!("打开麦克风失败: {error}"))?;

    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_and_the_short_tap_rule() {
        let audio = |frames: usize| RecordedAudio {
            samples: vec![0.0; frames],
            sample_rate: 48_000,
            truncated: false,
        };

        assert_eq!(audio(48_000).duration_ms(), 1_000);
        assert_eq!(audio(14_400).duration_ms(), 300);
        assert!(audio(0).is_too_short(), "一点没录到不算语音");
        assert!(audio(14_399).is_too_short(), "299ms 是轻点，不该发送");
        assert!(!audio(14_400).is_too_short(), "300ms 起正常发送");

        // 采样率为 0 时不能除零
        assert_eq!(
            RecordedAudio {
                samples: vec![1.0; 10],
                sample_rate: 0,
                truncated: false,
            }
            .duration_ms(),
            0
        );
    }

    #[test]
    fn downmixes_channels_and_stops_at_the_limit() {
        let buffer = Mutex::new(Vec::new());

        // 立体声：左右取平均
        push_samples(&[1.0f32, 0.0, -1.0, 1.0], 2, &buffer, 100);
        assert_eq!(buffer.lock().unwrap().as_slice(), &[0.5, 0.0]);

        // 单声道原样收下
        push_samples(&[0.25f32, -0.25], 1, &buffer, 3);
        assert_eq!(buffer.lock().unwrap().as_slice(), &[0.5, 0.0, 0.25]);

        // 到上限就不再收：内存必须有上界
        push_samples(&[1.0f32, 1.0], 1, &buffer, 3);
        assert_eq!(buffer.lock().unwrap().len(), 3);

        // 小于一个整帧的尾巴按现有通道数平均，不能越界
        let tail = Mutex::new(Vec::new());
        push_samples(&[0.4f32], 2, &tail, 10);
        assert_eq!(tail.lock().unwrap().as_slice(), &[0.4]);
    }

    #[test]
    fn writes_a_mono_16_bit_wav() {
        let dir = std::env::temp_dir().join(format!("bongo-cat-voice-{}", uuid::Uuid::new_v4()));

        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("voice.wav");
        let audio = RecordedAudio {
            samples: vec![0.0, 0.5, -0.5, 1.5, -1.5],
            sample_rate: 44_100,
            truncated: false,
        };

        write_wav(&path, &audio).unwrap();

        let mut reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();

        assert_eq!(spec.channels, 1, "R16：立体声要降混成单声道");
        assert_eq!(
            spec.sample_rate, 44_100,
            "R16：按设备原生采样率，不做重采样"
        );
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(reader.duration(), audio.samples.len() as u32);

        let samples: Vec<i16> = reader
            .samples::<i16>()
            .map(|sample| sample.unwrap())
            .collect();

        // 超出 [-1, 1] 的样本要夹住，不能绕回成噪音
        assert_eq!(samples[0], 0);
        assert_eq!(samples[3], i16::MAX);
        assert_eq!(samples[4], -i16::MAX);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wav_size_matches_what_hound_writes() {
        let audio = RecordedAudio {
            samples: vec![0.25; 4_800],
            sample_rate: 48_000,
            truncated: false,
        };
        let dir = std::env::temp_dir().join(format!("bongo-cat-voice-{}", uuid::Uuid::new_v4()));

        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("voice.wav");

        write_wav(&path, &audio).unwrap();

        // 落盘前按这个长度判断上限，算错就会要么误拦要么留下孤儿文件
        assert_eq!(wav_size(&audio), std::fs::metadata(&path).unwrap().len());

        // 1 MB 上限下 48kHz 大约只够 10 秒
        assert_eq!(wav_limit_secs(1024 * 1024, 48_000), 10);
        assert_eq!(wav_limit_secs(1024 * 1024, 0), 0, "采样率为 0 不能除零");
        assert_eq!(
            wav_limit_secs(1024 * 1024, 768_000),
            0,
            "极端采样率下装不下 1 秒，由调用方换成另一句文案"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 一个「已经自己结束」的录音：线程立刻返回
    fn finished_recording() -> Recording {
        let handle = std::thread::spawn(|| {
            Ok(RecordedAudio {
                samples: Vec::new(),
                sample_rate: 48_000,
                truncated: true,
            })
        });

        while !handle.is_finished() {
            std::thread::yield_now();
        }

        Recording {
            stop: Arc::new(AtomicBool::new(false)),
            handle,
        }
    }

    #[test]
    fn a_finished_recording_gives_the_slot_back() {
        // 到 60 秒上限就自己结束的那次录音：必须让位，否则之后再也录不了音
        let mut slot = Some(finished_recording());

        drop_finished(&mut slot);

        assert!(slot.is_none(), "自己结束的录音要收掉");

        // 还在录的不能被误收
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
            }

            Ok(RecordedAudio {
                samples: Vec::new(),
                sample_rate: 48_000,
                truncated: false,
            })
        });
        let mut live = Some(Recording {
            stop: Arc::clone(&stop),
            handle,
        });

        drop_finished(&mut live);

        assert!(live.is_some(), "还在录的录音不能被收掉");

        let _ = live.unwrap().finish();
    }

    /// 真机冒烟（§44 / R16）：默认麦克风能打开、能录到样本、写出的 WAV 读得回来
    ///
    /// 需要真实录音设备，所以默认忽略：
    /// `cargo test --lib pair::audio -- --ignored --nocapture`
    #[test]
    #[ignore = "需要真实麦克风"]
    fn records_from_the_default_device() {
        let recorder = Recorder::default();

        let sample_rate = match recorder.start() {
            Ok(sample_rate) => sample_rate,
            Err(error) => {
                eprintln!("跳过：默认麦克风打不开（{error}）");

                return;
            }
        };

        assert!(sample_rate > 0, "采样率应当来自设备（R16：按原生采样率录）");

        std::thread::sleep(Duration::from_millis(MIN_RECORDING_MS + 400));

        let recording = recorder.take().expect("刚才已经录上了");
        let audio = recording.finish().unwrap();

        assert_eq!(audio.sample_rate, sample_rate);
        assert!(!audio.is_too_short(), "录了 {}ms", audio.duration_ms());

        let dir = std::env::temp_dir().join(format!("bongo-cat-voice-{}", uuid::Uuid::new_v4()));

        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("voice.wav");

        write_wav(&path, &audio).unwrap();

        let reader = hound::WavReader::open(&path).unwrap();

        assert_eq!(reader.spec().channels, 1, "降混成单声道");
        assert!(reader.len() > 0, "录音文件不该是空的");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
