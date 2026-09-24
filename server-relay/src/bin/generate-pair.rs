//! 生成一个新的「联机密钥」（与客户端设置页的「生成联机密钥」等价）。
//!
//! ```text
//! cargo run --release --bin generate-pair
//! ```
//!
//! 多会话之后**服务器没有密钥**：这个值只是给两个人的客户端填的，填同一个值的两个人
//! 会落进同一个双人会话。中继只会看到从它派生出来的 `ROOM_ID` 与 `PAIR_AUTH_TOKEN`。
//!
//! 所以这个工具**不写 .env、也不碰服务器配置**——它只是给「手上没有图形界面」的场景
//! 留一条命令行生成的路。凭据纪律不变（R10）：派生结果不会被打印，生成的密钥只会出现在
//! 这一次的终端输出里，发完可以清屏。

use bongocat_pair_relay::auth;
use rand::Rng as _;

/// 同时打印「这个密钥属于哪个会话」的指纹（§29：日志与终端都不输出完整 ROOM_ID）
fn room_hint(secret: &[u8; auth::PAIR_SECRET_BYTES]) -> String {
    auth::room_fingerprint(&auth::derive_room_id(secret))
}

fn main() {
    let mut bytes = [0u8; auth::PAIR_SECRET_BYTES];

    rand::rng().fill_bytes(&mut bytes);

    let encoded = auth::encode_pair_secret(&bytes);
    let fingerprint = auth::fingerprint(&bytes);
    let room = room_hint(&bytes);

    println!();
    println!("联机密钥（两个人填**同一个**值，请自己保存后发给对方）：");
    println!();
    println!("  {encoded}");
    println!();
    println!("核对指纹（双方在设置页里比对，一致才算填对了）：{fingerprint}");
    println!("会话指纹（只用于对日志，不能反推密钥）：{room}");
    println!();
    println!("提醒：上面这行会留在终端回滚区，发给对方后可以清屏（cls / clear）。");
    println!("服务器不需要它，也没有任何服务器配置要改。");
    println!();
}
