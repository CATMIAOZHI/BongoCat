//! 生成「配对密码」（与客户端设置页的「生成配对密码」等价），以及服务器密码。
//!
//! ```text
//! cargo run --release --bin generate-pair            # 配对密码
//! cargo run --release --bin generate-pair -- --server # 服务器密码
//! cargo run --release --bin generate-pair -- --all    # 两个都要（新部署时最省事）
//! ```
//!
//! 两个值的作用完全不同（R36）：
//!
//! - **配对密码**：给两个人的客户端填的，填同一个值的两个人会落进同一个双人会话。
//!   中继只会看到从它派生出来的 `ROOM_ID` 与 `PAIR_AUTH_TOKEN`。
//! - **服务器密码**：部署者在服务器上设置的（`.env` 的 `PAIR_SERVER_PASSWORD`），
//!   决定「谁能用这台服务器」。两个客户端也要填同一个值（设置页的「服务器密码」）。
//!   它挡的是「陌生人拿你的服务器当免费转发 + 白拿 TURN 凭据」，与配对无关。
//!
//! 所以这个工具**不写 .env、也不碰服务器配置**——它只是给「手上没有图形界面」的场景
//! 留一条命令行生成的路。凭据纪律不变（R10）：派生结果不会被打印，生成的值只会出现在
//! 这一次的终端输出里，发完可以清屏。

use bongocat_pair_relay::auth;
use bongocat_pair_relay::protocol::MIN_SERVER_PASSWORD_LENGTH;
use rand::Rng as _;

/// 服务器密码的长度：取 24 字节随机数，逐个映射到 57 个**去掉易混字符（I / O / l）**
/// 的字母数字字符集，得到 24 个字符（约 139 bit 熵，远超 16 字符的最小长度要求）。
const SERVER_PASSWORD_BYTES: usize = 24;

/// 同时打印「这个密钥属于哪个会话」的指纹（§29：日志与终端都不输出完整 ROOM_ID）
fn room_hint(secret: &[u8; auth::PAIR_SECRET_BYTES]) -> String {
    auth::room_fingerprint(&auth::derive_room_id(secret))
}

fn print_pair_secret() {
    let mut bytes = [0u8; auth::PAIR_SECRET_BYTES];

    rand::rng().fill_bytes(&mut bytes);

    let encoded = auth::encode_pair_secret(&bytes);
    let fingerprint = auth::fingerprint(&bytes);
    let room = room_hint(&bytes);

    println!();
    println!("配对密码（两个人填**同一个**值，请自己保存后发给对方）：");
    println!();
    println!("  {encoded}");
    println!();
    println!("核对指纹（双方在设置页里比对，一致才算填对了）：{fingerprint}");
    println!("会话指纹（只用于对日志，不能反推密码）：{room}");
    println!();
    println!("提醒：上面这行会留在终端回滚区，发给对方后可以清屏（cls / clear）。");
    println!("服务器不需要它——服务器要的是下面那个「服务器密码」。");
    println!();
}

/// 服务器密码：**任意文本**（不是 32 字节的 base64），所以这里只生成一串足够长的随机字符
fn print_server_password() {
    let mut bytes = [0u8; SERVER_PASSWORD_BYTES];

    rand::rng().fill_bytes(&mut bytes);

    let password: String = bytes
        .iter()
        .map(|byte| {
            char::from(
                b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789"[(*byte as usize) % 57],
            )
        })
        .collect();

    println!();
    println!("服务器密码（写在服务器的 .env 里：PAIR_SERVER_PASSWORD=…）：");
    println!();
    println!("  {password}");
    println!();
    println!(
        "把它填进 .env 后重启中继，两个客户端的「服务器密码」都填这一个值。\
         没有它的人连不上你的服务器（也拿不到 TURN 凭据）。"
    );
    println!(
        "长度 {} 个字符，满足最小 {MIN_SERVER_PASSWORD_LENGTH} 字符的要求；自己也可以换成任何\
         长度足够的密码。",
        password.chars().count()
    );
    println!();
    println!("提醒：换掉这个值＝所有人都要重新填一次；它只是门槛，不是端到端加密的一部分。");
    println!();
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();

    match mode.as_str() {
        "" => print_pair_secret(),
        "--server" => print_server_password(),
        "--all" => {
            print_server_password();
            print_pair_secret();
        }
        other => {
            eprintln!("未知参数 {other:?}：可用的是（不带参数）/ --server / --all");
            std::process::exit(2);
        }
    }
}
