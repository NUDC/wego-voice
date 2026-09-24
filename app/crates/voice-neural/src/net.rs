//! HTTPS 下载 —— 走 **WinHTTP**，不拉 HTTP 客户端库。
//!
//! # 为什么是 Win32 FFI 而不是 `reqwest`/`ureq`
//!
//! 下载必须在**主程序**里（伴生程序本身也要下，鸡生蛋），而主程序的卖点
//! 是 6.5 MB 免安装单文件。带 TLS 的 HTTP 客户端要 2 MB 上下，
//! 为一个大多数人不会用到的功能让**所有人**付这个体积，不划算。
//!
//! WinHTTP 是系统 DLL，**体积成本为零**。而且它自带两件在中国的
//! Windows 上很要紧的事：
//!
//! - **走系统代理设置**。公司网、加速器改的都是系统代理，
//!   自带 TLS 栈的库默认不读它，表现是"别的软件能下，就它不行"。
//! - **用系统证书库**。装了企业根证书的机器，自带证书库的客户端会
//!   握手失败。
//!
//! 这个项目已经在用同样的手法（`MessageBoxW`、`SHFileOperationW`、
//! `GetLogicalDrives`），不算新增复杂度。
//!
//! # 断点续传是必须的，不是加分项
//!
//! 单个文件 360 MB。这台开发机直连 GitHub Release 实测多次超时 ——
//! 用户的网只会更差。不支持续传的话，下到 90% 断一次就得从头再来，
//! 而那种体验会让人直接放弃这个功能。

use anyhow::{bail, Context, Result};
use std::path::Path;

/// 拆开的 URL。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// 含前导 `/`，带查询串。
    pub path: String,
}

/// 拆 URL。
///
/// 抽成纯函数是为了**能测** —— WinHTTP 那部分测不了，但拆错 URL
/// （端口丢了、路径少了前导斜杠、查询串被吃掉）是最容易犯又最难查的错：
/// 表现是 404 或者连不上，看不出是自己拼错了。
pub fn parse_url(s: &str) -> Result<Url> {
    let (https, rest) = if let Some(r) = s.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = s.strip_prefix("http://") {
        (false, r)
    } else {
        bail!("只支持 http/https：{s}");
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        bail!("URL 里没有主机名：{s}");
    }
    let (host, port) = match authority.rsplit_once(':') {
        // IPv6 不支持，也用不上
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().with_context(|| format!("端口不是数字：{p}"))?,
        ),
        None => (authority.to_string(), if https { 443 } else { 80 }),
    };
    Ok(Url { https, host, port, path: path.to_string() })
}

/// 下载进度。
pub struct Progress {
    /// 已经拿到的字节数（含续传前就有的）。
    pub have: u64,
    /// 预期总字节数。
    pub total: u64,
}

/// 把 `url` 下载到 `dest`，**支持断点续传**。
///
/// `total` 是预期字节数（来自资产表）—— 有它就不必去解析 `Content-Length`，
/// 少一处可能出错的地方。
///
/// `progress` 返回 `false` 表示取消；此时已下的部分**保留**，下次接着下。
#[cfg(windows)]
pub fn download(
    url: &str,
    dest: &Path,
    total: u64,
    mut progress: impl FnMut(Progress) -> bool,
) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    let u = parse_url(url)?;
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d).ok();
    }

    // 已经下了多少 —— 从这里接着要
    let have = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    if have >= total && total > 0 {
        return Ok(());
    }

    let session = Session::open()?;
    let conn = session.connect(&u.host, u.port)?;
    let req = conn.request(&u.path, u.https)?;
    if have > 0 {
        req.add_header(&format!("Range: bytes={have}-"))?;
    }
    req.send()?;
    let status = req.status()?;

    // 206 = 接受了续传；200 = 服务器不认 Range，只能从头来
    let restart = match status {
        206 => false,
        200 => true,
        416 => {
            // Range 越界：本地比服务器还大，说明文件对不上，重下
            true
        }
        s => bail!("服务器返回 HTTP {s}：{url}"),
    };

    let mut f = if restart || have == 0 {
        std::fs::File::create(dest).with_context(|| format!("创建失败：{}", dest.display()))?
    } else {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(dest)
            .with_context(|| format!("打开失败：{}", dest.display()))?;
        f.seek(SeekFrom::Start(have)).context("定位失败")?;
        f
    };
    let mut done = if restart { 0 } else { have };

    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = req.read(&mut buf)?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n]).context("写入失败")?;
        done += n as u64;
        if !progress(Progress { have: done, total }) {
            // 取消：**保留已下的部分**，下次接着下
            f.flush().ok();
            bail!("已取消");
        }
    }
    f.flush().context("刷新失败")?;
    Ok(())
}

#[cfg(not(windows))]
pub fn download(
    _url: &str,
    _dest: &Path,
    _total: u64,
    _progress: impl FnMut(Progress) -> bool,
) -> Result<()> {
    bail!("只在 Windows 上支持下载")
}

// ───────────────────────── WinHTTP 薄封装 ─────────────────────────
//
// 只包到"够用"为止。每个句柄都有 Drop，避免漏 —— 漏了的表现是
// 下几次之后再也连不上，而那时候根本想不到是句柄泄漏。

#[cfg(windows)]
mod sys {
    use anyhow::Result;
    use std::ffi::c_void;

    type Handle = *mut c_void;

    #[link(name = "winhttp")]
    unsafe extern "system" {
        fn WinHttpOpen(
            agent: *const u16,
            access_type: u32,
            proxy: *const u16,
            bypass: *const u16,
            flags: u32,
        ) -> Handle;
        fn WinHttpConnect(session: Handle, server: *const u16, port: u16, reserved: u32) -> Handle;
        fn WinHttpOpenRequest(
            connect: Handle,
            verb: *const u16,
            object: *const u16,
            version: *const u16,
            referrer: *const u16,
            accept_types: *const *const u16,
            flags: u32,
        ) -> Handle;
        fn WinHttpAddRequestHeaders(
            request: Handle,
            headers: *const u16,
            len: u32,
            modifiers: u32,
        ) -> i32;
        fn WinHttpSendRequest(
            request: Handle,
            headers: *const u16,
            headers_len: u32,
            optional: *mut c_void,
            optional_len: u32,
            total_len: u32,
            context: usize,
        ) -> i32;
        fn WinHttpReceiveResponse(request: Handle, reserved: *mut c_void) -> i32;
        fn WinHttpQueryHeaders(
            request: Handle,
            info_level: u32,
            name: *const u16,
            buffer: *mut c_void,
            buffer_len: *mut u32,
            index: *mut u32,
        ) -> i32;
        fn WinHttpReadData(
            request: Handle,
            buffer: *mut c_void,
            to_read: u32,
            read: *mut u32,
        ) -> i32;
        fn WinHttpCloseHandle(h: Handle) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLastError() -> u32;
    }

    /// 走**系统代理设置**。公司网、加速器改的都是这个 ——
    /// 不读它的话表现是"别的软件能下，就它不行"。
    const ACCESS_TYPE_DEFAULT_PROXY: u32 = 0;
    const FLAG_SECURE: u32 = 0x0080_0000;
    const ADDREQ_FLAG_ADD: u32 = 0x2000_0000;
    const QUERY_STATUS_CODE: u32 = 19;
    const QUERY_FLAG_NUMBER: u32 = 0x2000_0000;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    fn last_error(what: &str) -> anyhow::Error {
        let code = unsafe { GetLastError() };
        // 几个最常见的，给人话；其余报码
        let hint = match code {
            12007 => "解析不了主机名（检查网络或 DNS）",
            12029 => "连不上服务器",
            12002 => "超时",
            12175 => "TLS 握手失败（检查系统时间与证书）",
            _ => "",
        };
        if hint.is_empty() {
            anyhow::anyhow!("{what} 失败（WinHTTP 错误 {code}）")
        } else {
            anyhow::anyhow!("{what} 失败：{hint}（WinHTTP 错误 {code}）")
        }
    }

    pub struct Session(Handle);
    pub struct Connect(Handle);
    pub struct Request(Handle);

    // 句柄只在建它的线程上用，这里只是为了能放进 struct
    unsafe impl Send for Session {}
    unsafe impl Send for Connect {}
    unsafe impl Send for Request {}

    impl Drop for Session {
        fn drop(&mut self) {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
    impl Drop for Connect {
        fn drop(&mut self) {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
    impl Drop for Request {
        fn drop(&mut self) {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }

    impl Session {
        pub fn open() -> Result<Self> {
            let agent = wide("wego-voice");
            let h = unsafe {
                WinHttpOpen(
                    agent.as_ptr(),
                    ACCESS_TYPE_DEFAULT_PROXY,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            };
            if h.is_null() {
                return Err(last_error("初始化网络"));
            }
            Ok(Self(h))
        }

        pub fn connect(&self, host: &str, port: u16) -> Result<Connect> {
            let h = unsafe { WinHttpConnect(self.0, wide(host).as_ptr(), port, 0) };
            if h.is_null() {
                return Err(last_error(&format!("连接 {host}")));
            }
            Ok(Connect(h))
        }
    }

    impl Connect {
        pub fn request(&self, path: &str, https: bool) -> Result<Request> {
            let verb = wide("GET");
            let obj = wide(path);
            let h = unsafe {
                WinHttpOpenRequest(
                    self.0,
                    verb.as_ptr(),
                    obj.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    if https { FLAG_SECURE } else { 0 },
                )
            };
            if h.is_null() {
                return Err(last_error("建立请求"));
            }
            Ok(Request(h))
        }
    }

    impl Request {
        pub fn add_header(&self, line: &str) -> Result<()> {
            let w = wide(line);
            let ok = unsafe {
                WinHttpAddRequestHeaders(self.0, w.as_ptr(), u32::MAX, ADDREQ_FLAG_ADD)
            };
            if ok == 0 {
                return Err(last_error("设置请求头"));
            }
            Ok(())
        }

        pub fn send(&self) -> Result<()> {
            let ok = unsafe {
                WinHttpSendRequest(self.0, std::ptr::null(), 0, std::ptr::null_mut(), 0, 0, 0)
            };
            if ok == 0 {
                return Err(last_error("发送请求"));
            }
            let ok = unsafe { WinHttpReceiveResponse(self.0, std::ptr::null_mut()) };
            if ok == 0 {
                return Err(last_error("接收响应"));
            }
            Ok(())
        }

        pub fn status(&self) -> Result<u32> {
            let mut code: u32 = 0;
            let mut len = std::mem::size_of::<u32>() as u32;
            let ok = unsafe {
                WinHttpQueryHeaders(
                    self.0,
                    QUERY_STATUS_CODE | QUERY_FLAG_NUMBER,
                    std::ptr::null(),
                    &mut code as *mut u32 as *mut std::ffi::c_void,
                    &mut len,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(last_error("读取状态码"));
            }
            Ok(code)
        }

        pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
            let mut n: u32 = 0;
            let ok = unsafe {
                WinHttpReadData(
                    self.0,
                    buf.as_mut_ptr() as *mut std::ffi::c_void,
                    buf.len() as u32,
                    &mut n,
                )
            };
            if ok == 0 {
                return Err(last_error("读取数据"));
            }
            Ok(n as usize)
        }
    }
}

#[cfg(windows)]
use sys::Session;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ordinary_urls() {
        let u = parse_url("https://example.com/a/b.bin").unwrap();
        assert!(u.https);
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/a/b.bin");
    }

    /// 查询串必须留着。GitHub 的下载直链带签名参数，
    /// 吃掉它的表现是 403 —— 看着像没权限，其实是自己拼丢了。
    #[test]
    fn keeps_the_query_string() {
        let u = parse_url("https://h.com/x?token=abc&y=1").unwrap();
        assert_eq!(u.path, "/x?token=abc&y=1");
    }

    #[test]
    fn honours_an_explicit_port() {
        let u = parse_url("http://h.com:8080/x").unwrap();
        assert!(!u.https);
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/x");
    }

    /// 没有路径时要补上前导斜杠，否则 WinHTTP 拿到空对象名。
    #[test]
    fn a_bare_host_gets_a_root_path() {
        assert_eq!(parse_url("https://h.com").unwrap().path, "/");
    }

    #[test]
    fn rejects_what_it_cannot_handle() {
        assert!(parse_url("ftp://h.com/x").is_err());
        assert!(parse_url("h.com/x").is_err());
        assert!(parse_url("https://").is_err());
        assert!(parse_url("https://h.com:abc/x").is_err());
    }
}
