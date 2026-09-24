//! 联机功能的两个凭据在本地的保存：配对密码与服务器密码（R36）。
//!
//! 只存进系统凭据库（Windows Credential Manager / macOS Keychain / Linux Secret
//! Service），绝不写进 JSON 设置文件、Pinia、localStorage 或日志。前端只能写入、
//! 查询是否存在、删除，不能读回明文。

const SERVICE: &str = "com.ayangweb.BongoCat.pair";
/// 两个人的共享凭据（决定「谁是同一对」，也是 E2EE 密钥材料）
const ACCOUNT_SECRET: &str = "pair-secret";
/// 服务器的门槛凭据（决定「能不能用这台服务器」，由部署者设置）
const ACCOUNT_SERVER_PASSWORD: &str = "server-password";

fn entry(account: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, account).map_err(|err| format!("无法访问系统凭据库: {err}"))
}

pub fn set_secret(secret: &str) -> Result<(), String> {
    set(ACCOUNT_SECRET, secret, "保存配对密码失败")
}

pub fn has_secret() -> Result<bool, String> {
    Ok(load_secret()?.is_some())
}

pub fn load_secret() -> Result<Option<String>, String> {
    load(ACCOUNT_SECRET, "读取配对密码失败")
}

pub fn delete_secret() -> Result<(), String> {
    delete(ACCOUNT_SECRET, "删除配对密码失败")
}

/// 服务器密码（R36）：与配对密码分两个条目存，互不覆盖——改服务器密码不该让人
/// 重新填一次配对密码（那会连带换掉 E2EE 密钥材料）。
pub fn set_server_password(password: &str) -> Result<(), String> {
    set(ACCOUNT_SERVER_PASSWORD, password, "保存服务器密码失败")
}

pub fn has_server_password() -> Result<bool, String> {
    Ok(load_server_password()?.is_some())
}

pub fn load_server_password() -> Result<Option<String>, String> {
    load(ACCOUNT_SERVER_PASSWORD, "读取服务器密码失败")
}

pub fn delete_server_password() -> Result<(), String> {
    delete(ACCOUNT_SERVER_PASSWORD, "删除服务器密码失败")
}

fn set(account: &str, value: &str, failure: &str) -> Result<(), String> {
    entry(account)?
        .set_password(value.trim())
        .map_err(|err| format!("{failure}: {err}"))
}

fn load(account: &str, failure: &str) -> Result<Option<String>, String> {
    match entry(account)?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(format!("{failure}: {err}")),
    }
}

fn delete(account: &str, failure: &str) -> Result<(), String> {
    match entry(account)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(format!("{failure}: {err}")),
    }
}
