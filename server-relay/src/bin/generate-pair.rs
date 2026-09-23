//! 生成一对用户的共享密钥（与 `server-cloudflare/scripts/generate-pair.mjs` 等价）。
//!
//! ```text
//! cargo run --release --bin generate-pair              只生成并显示 Pair Secret
//! cargo run --release --bin generate-pair -- --write   额外写入 .env（已 gitignore）
//! ```
//!
//! 分工：
//!   PAIR_SECRET     —— 由用户自己保存并交给对方（双方客户端填同一个值）
//!   PAIR_AUTH_TOKEN —— 从 PAIR_SECRET 派生，只存在于中继进程的内存里
//!
//! **派生出的 token 不会被打印**：根据项目约定（R10），凭据不进 stdout、不进
//! shell history、不落盘。中继启动时会自己从 `PAIR_SECRET` 派生。

use std::fs::OpenOptions;
use std::io::Write as _;

use bongocat_pair_relay::auth;
use rand::Rng as _;

/// 相对**当前目录**，所以请在 `server-relay/` 里执行（README 的步骤 1 有写）
const SECRET_FILE: &str = ".env";

fn main() {
    let mut bytes = [0u8; auth::PAIR_SECRET_BYTES];

    rand::rng().fill_bytes(&mut bytes);

    let encoded = auth::encode_pair_secret(&bytes);
    let fingerprint = auth::fingerprint(&bytes);

    println!();
    println!("Pair Secret（双方要填同一个值，请自己保存后发给对方）：");
    println!();
    println!("  {encoded}");
    println!();
    println!("指纹（双方配置后可比对，只用于核对是否填了同一个 secret）：{fingerprint}");
    println!();
    println!("提醒：上面这行会留在终端回滚区，发给对方后可以清屏（cls / clear）。");
    println!("中继只保存这个值（或它派生出的 token），派生结果不会被打印、也不会落盘。");
    println!();

    if std::env::args().any(|argument| argument == "--write") {
        // create_new：绝不覆盖已存在的 .env（里面可能已经有 PAIR_DOMAIN 等配置），
        // 也不会把 secret 写到一个已经有了同名文件的目录里
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(SECRET_FILE)
        {
            Ok(mut file) => {
                let body = format!("# 由 generate-pair 生成，请勿提交\nPAIR_SECRET={encoded}\n");

                if let Err(error) = file.write_all(body.as_bytes()) {
                    eprintln!("写入 {SECRET_FILE} 失败：{error}");
                    std::process::exit(1);
                }

                println!("已写入 {SECRET_FILE}（`server-relay/.gitignore` 里已忽略）。");
                println!("还需要补一行 PAIR_DOMAIN=<你的域名>。");
                println!("不需要时请删除：Remove-Item {SECRET_FILE}  或  rm {SECRET_FILE}");
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                println!("{SECRET_FILE} 已存在，**没有覆盖**。请手动把下面这行加进去：");
                println!();
                println!("  PAIR_SECRET={encoded}");
                println!();
            }
            Err(error) => {
                eprintln!("写入 {SECRET_FILE} 失败：{error}");
                std::process::exit(1);
            }
        }
    } else {
        println!("下一步：");
        println!("  把下面这行写进 server-relay/.env（在 server-relay/ 目录里执行，");
        println!("  且不要写进命令行参数，避免进 shell history）：");
        println!();
        println!("    PAIR_SECRET={encoded}");
        println!();
        println!("  再补一行 PAIR_DOMAIN=<你的域名>，然后 docker compose up -d；");
        println!("  客户端填 https://<你的域名> 与上面那个 Pair Secret。");
    }

    println!();
}
