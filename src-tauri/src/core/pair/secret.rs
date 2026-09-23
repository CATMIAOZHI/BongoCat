//! Pair Secret 的本地保存。
//!
//! 只存进系统凭据库（Windows Credential Manager / macOS Keychain / Linux Secret
//! Service），绝不写进 JSON 设置文件、Pinia、localStorage 或日志。前端只能写入、
//! 查询是否存在、删除，不能读回明文。

const SERVICE: &str = "com.ayangweb.BongoCat.pair";
const ACCOUNT: &str = "pair-secret";

fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, ACCOUNT).map_err(|err| format!("无法访问系统凭据库: {err}"))
}

pub fn set_secret(secret: &str) -> Result<(), String> {
    entry()?
        .set_password(secret.trim())
        .map_err(|err| format!("保存 Pair Secret 失败: {err}"))
}

pub fn has_secret() -> Result<bool, String> {
    Ok(load_secret()?.is_some())
}

pub fn load_secret() -> Result<Option<String>, String> {
    match entry()?.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(format!("读取 Pair Secret 失败: {err}")),
    }
}

pub fn delete_secret() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(format!("删除 Pair Secret 失败: {err}")),
    }
}
