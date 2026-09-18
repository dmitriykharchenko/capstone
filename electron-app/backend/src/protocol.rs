use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub const VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_ACTIVE_REQUESTS: usize = 32;
pub const MAX_SESSION_REQUESTS: usize = 4096;
pub const QUEUE_CAPACITY: usize = 32;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Input {
    Request {
        v: u32,
        id: String,
        command: String,
        params: Value,
    },
    Cancel {
        v: u32,
        id: String,
    },
}

impl Input {
    pub fn parse(bytes: &[u8]) -> Result<Self, ErrorCode> {
        let input: Self = serde_json::from_slice(bytes).map_err(|_| ErrorCode::InvalidRequest)?;
        let (v, id) = match &input {
            Self::Request {
                v,
                id,
                command,
                params,
            } => {
                if command.is_empty() || command.len() > 64 || !params.is_object() {
                    return Err(ErrorCode::InvalidRequest);
                }
                (*v, id)
            }
            Self::Cancel { v, id } => (*v, id),
        };
        if v != VERSION {
            return Err(ErrorCode::UnsupportedVersion);
        }
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
        {
            return Err(ErrorCode::InvalidRequest);
        }
        Ok(input)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    UnsupportedVersion,
    FrameTooLarge,
    TruncatedFrame,
    DuplicateId,
    SessionLimit,
    Busy,
    UnknownCommand,
    Unavailable,
    InvalidParams,
    Internal,
    ResponseTooLarge,
    Cancelled,
}

impl ErrorCode {
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidRequest => "Invalid protocol request.",
            Self::UnsupportedVersion => "Unsupported protocol version.",
            Self::FrameTooLarge => "Input frame exceeds the byte limit.",
            Self::TruncatedFrame => "Input ended before the frame delimiter.",
            Self::DuplicateId => "Request IDs must not be reused within a connection.",
            Self::SessionLimit => "Connection request limit reached; reconnect.",
            Self::Busy => "Too many active requests.",
            Self::UnknownCommand => "Unknown command.",
            Self::Unavailable => "This capability is not implemented in the foundation build.",
            Self::InvalidParams => "Invalid command parameters.",
            Self::Internal => "Backend operation failed.",
            Self::ResponseTooLarge => "Output frame exceeds the byte limit; use bounded chunks.",
            Self::Cancelled => "Request cancelled.",
        }
    }
}

// Deliberately cannot carry provider errors, credentials, paths, prompts or panic text.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PublicError {
    pub code: ErrorCode,
    pub message: String,
}

impl From<ErrorCode> for PublicError {
    fn from(code: ErrorCode) -> Self {
        Self {
            code,
            message: code.message().to_owned(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Output {
    pub v: u32,
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(flatten)]
    pub body: OutputBody,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputBody {
    Event { event: String, data: Value },
    Result { data: Value },
    Error { error: PublicError },
    Cancelled,
    ProtocolError { error: PublicError },
    CancelAck { accepted: bool },
}

impl Output {
    pub fn request(id: &str, seq: u64, body: OutputBody) -> Self {
        Self {
            v: VERSION,
            id: Some(id.to_owned()),
            seq: Some(seq),
            body,
        }
    }

    pub fn protocol_error(code: ErrorCode) -> Self {
        Self {
            v: VERSION,
            id: None,
            seq: None,
            body: OutputBody::ProtocolError { error: code.into() },
        }
    }
}

// Drain oversized frames without ever allocating their unbounded contents.
pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Result<Vec<u8>, ErrorCode>>> {
    let mut frame = Vec::new();
    let mut oversized = false;
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return Ok(if oversized || !frame.is_empty() {
                Some(Err(ErrorCode::TruncatedFrame))
            } else {
                None
            });
        }
        let end = bytes.iter().position(|b| *b == b'\n');
        let length = end.unwrap_or(bytes.len());
        if !oversized {
            if frame.len() + length > MAX_FRAME_BYTES {
                oversized = true;
                frame.clear();
            } else {
                frame.extend_from_slice(&bytes[..length]);
            }
        }
        reader.consume(length + usize::from(end.is_some()));
        if end.is_some() {
            return Ok(Some(if oversized {
                Err(ErrorCode::FrameTooLarge)
            } else {
                Ok(frame)
            }));
        }
    }
}
