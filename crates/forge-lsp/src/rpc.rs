use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdout;

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
    use tokio::io::{duplex, BufReader};

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
}
