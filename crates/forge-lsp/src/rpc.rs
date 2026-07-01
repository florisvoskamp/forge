use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdout;

/// Upper bound on a single LSP message body. Real messages (even large `publishDiagnostics`
/// batches) are nowhere near this; it exists purely to reject a corrupt/misbehaving server's
/// bogus `Content-Length` before we `vec![0u8; len]` it, which would otherwise abort the whole
/// process on allocation failure instead of degrading to "no diagnostics".
const MAX_CONTENT_LENGTH: usize = 16 * 1024 * 1024;

pub async fn write_msg<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Value,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

pub(crate) async fn read_msg_inner<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Option<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return None,
            _ => {}
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length: ") {
            content_length = val.trim().parse().ok();
        }
    }
    let len = content_length?;
    if len > MAX_CONTENT_LENGTH {
        return None;
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.ok()?;
    serde_json::from_slice(&buf).ok()
}

pub async fn read_msg(reader: &mut BufReader<ChildStdout>) -> Option<Value> {
    read_msg_inner(reader).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncWriteExt, BufReader};

    /// Frame `bytes` through a closed in-memory stream and return what `read_msg_inner` makes of it.
    async fn parse_bytes(bytes: &[u8]) -> Option<Value> {
        let (mut writer, reader) = duplex(64 * 1024);
        writer.write_all(bytes).await.unwrap();
        drop(writer);
        let mut reader = BufReader::new(reader);
        read_msg_inner(&mut reader).await
    }

    #[tokio::test]
    async fn round_trip() {
        let (mut writer, reader) = duplex(4096);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "test",
            "params": null
        });
        write_msg(&mut writer, &msg).await.unwrap();
        drop(writer);
        let mut buf_reader = BufReader::new(reader);
        let result = read_msg_inner(&mut buf_reader).await.expect("should parse");
        assert_eq!(result["method"], "test");
        assert_eq!(result["id"], 1);
    }

    #[tokio::test]
    async fn write_msg_emits_content_length_header() {
        let mut out: Vec<u8> = Vec::new();
        let msg = serde_json::json!({"a": 1});
        write_msg(&mut out, &msg).await.unwrap();
        let body = serde_json::to_vec(&msg).unwrap();
        let expected_header = format!("Content-Length: {}\r\n\r\n", body.len());
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with(&expected_header), "header was: {text:?}");
        assert!(text.ends_with(&String::from_utf8(body).unwrap()));
    }

    #[tokio::test]
    async fn eof_yields_none() {
        assert!(parse_bytes(b"").await.is_none());
    }

    #[tokio::test]
    async fn headers_without_content_length_yield_none() {
        // A complete header block (blank line) but no Content-Length => nothing to read.
        assert!(parse_bytes(b"Content-Type: application/json\r\n\r\n")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn invalid_content_length_yields_none() {
        assert!(parse_bytes(b"Content-Length: not-a-number\r\n\r\n{}")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn oversized_content_length_yields_none() {
        // A bogus/corrupt header claiming far more than MAX_CONTENT_LENGTH must be rejected
        // before allocating, not passed to `vec![0u8; len]`.
        let header = format!("Content-Length: {}\r\n\r\n", MAX_CONTENT_LENGTH + 1);
        assert!(parse_bytes(header.as_bytes()).await.is_none());
    }

    #[tokio::test]
    async fn partial_body_yields_none() {
        // Declares 100 bytes but only 2 follow => read_exact fails, must not panic.
        assert!(parse_bytes(b"Content-Length: 100\r\n\r\n{}")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn garbage_body_yields_none() {
        // 13-byte body that isn't valid JSON.
        assert!(parse_bytes(b"Content-Length: 13\r\n\r\nnot-json-here")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn ignores_extra_headers_and_parses_body() {
        let body = br#"{"jsonrpc":"2.0","id":7}"#;
        let mut bytes = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(body);
        let v = parse_bytes(&bytes).await.expect("should parse");
        assert_eq!(v["id"], 7);
    }

    #[tokio::test]
    async fn reads_only_declared_body_then_stops() {
        // Only `len` bytes belong to the first message; the rest is a second frame.
        let body = br#"{"id":1}"#;
        let mut bytes = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(b"Content-Length: 8\r\n\r\n{\"id\":2}");
        let v = parse_bytes(&bytes).await.expect("should parse first");
        assert_eq!(v["id"], 1);
    }

    #[tokio::test]
    async fn handles_multibyte_utf8_body() {
        // Content-Length is a BYTE count; a multibyte char must be framed by bytes, not chars.
        let (mut writer, reader) = duplex(4096);
        let msg = serde_json::json!({"msg": "café 你好"});
        write_msg(&mut writer, &msg).await.unwrap();
        drop(writer);
        let mut reader = BufReader::new(reader);
        let v = read_msg_inner(&mut reader).await.expect("should parse");
        assert_eq!(v["msg"], "café 你好");
    }
}
