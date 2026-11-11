//! The browser direct link: the web client's address with a join link in its fragment.
//!
//! `Why a fragment:` it is never sent to the web server, so the room key stays out of its logs.
//! `Why:` user request 2026-11-11 (right-click "Copy invite link" > "Copy browser direct link").
//! Format: `architecture/protocol-spec.md` §11.4.

/// The hosted browser client the direct link opens.
pub const WEB_CLIENT_URL: &str = "https://ritze03.github.io/simple-screensharing/";

/// `WEB_CLIENT_URL#join=<join link, percent-encoded>`. Everything except `A-Z a-z 0-9 - _ . ~ ! * ' ( )`
/// is encoded (JS `encodeURIComponent` decodes it back); `|`, `%`, `#`, `&`, space and non-ASCII always are.
pub fn direct_link(join_link: &str) -> String {
    let mut out = format!("{WEB_CLIENT_URL}#join=");
    for b in join_link.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'!' | b'*' | b'\'' | b'(' | b')') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_separators_and_non_ascii() {
        assert_eq!(
            direct_link("host:4433|ab12|k-_y|Zm9v|My Room é#&%"),
            "https://ritze03.github.io/simple-screensharing/#join=host%3A4433%7Cab12%7Ck-_y%7CZm9v%7CMy%20Room%20%C3%A9%23%26%25"
        );
    }
}
