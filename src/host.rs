pub(crate) fn short_hostname() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let full = if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    };
    match full.split('.').next() {
        Some(short) if !short.is_empty() => short.to_owned(),
        _ => "mado".to_owned(),
    }
}
