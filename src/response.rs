use ascii::AsciiString;
use chunked_transfer;
use encoding::label::encoding_from_whatwg_label;
use encoding::DecoderTrap;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Result as IoResult;

use error::Error;

const DEFAULT_CONTENT_TYPE: &'static str = "text/plain";
const DEFAULT_CHARACTER_SET: &'static str = "utf-8";

/// Response instances are created as results of firing off requests.
///
/// The `Response` is used to read response headers and decide what to do with the body.
/// Note that the socket connection is open and the body not read until one of
/// [`into_reader()`](#method.into_reader), [`into_json()`](#method.into_json) or
/// [`into_string()`](#method.into_string) consumes the response.
///
/// ```
/// let response = ureq::get("https://www.google.com").call();
///
/// // socket is still open and the response body has not been read.
///
/// let text = response.into_string().unwrap();
///
/// // response is consumed, and body has been read.
/// ```
pub struct Response {
    error: Option<Error>,
    status_line: AsciiString,
    index: (usize, usize), // index into status_line where we split: HTTP/1.1 200 OK
    status: u16,
    headers: Vec<Header>,
    stream: Option<Stream>,
}

impl ::std::fmt::Debug for Response {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::result::Result<(), ::std::fmt::Error> {
        write!(
            f,
            "Response[status: {}, status_text: {}]",
            self.status(),
            self.status_text()
        )
    }
}

impl Response {
    /// Construct a response with a status, status text and a string body.
    ///
    /// This is hopefully useful for unit tests.
    ///
    /// Example:
    ///
    /// ```
    /// let resp = ureq::Response::new(401, "Authorization Required", "Please log in");
    ///
    /// assert_eq!(*resp.status(), 401);
    /// ```
    pub fn new(status: u16, status_text: &str, body: &str) -> Self {
        let r = format!("HTTP/1.1 {} {}\r\n\r\n{}\n", status, status_text, body);
        (r.as_ref() as &str)
            .parse::<Response>()
            .unwrap_or_else(|e| e.into())
    }

    /// The entire status line like: `HTTP/1.1 200 OK`
    pub fn status_line(&self) -> &str {
        self.status_line.as_str()
    }

    /// The http version: `HTTP/1.1`
    pub fn http_version(&self) -> &str {
        &self.status_line.as_str()[0..self.index.0]
    }

    /// The status as a u16: `200`
    pub fn status(&self) -> &u16 {
        &self.status
    }

    /// The status text: `OK`
    pub fn status_text(&self) -> &str {
        &self.status_line.as_str()[self.index.1 + 1..].trim()
    }

    /// The header corresponding header value for the give name, if any.
    pub fn header<'a>(&self, name: &'a str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.is_name(name))
            .map(|h| h.value())
    }

    /// Tells if the response has the named header.
    pub fn has<'a>(&self, name: &'a str) -> bool {
        self.header(name).is_some()
    }

    /// All headers corresponding values for the give name, or empty vector.
    pub fn all<'a>(&self, name: &'a str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|h| h.is_name(name))
            .map(|h| h.value())
            .collect()
    }

    /// Whether the response status is: 200 <= status <= 299
    pub fn ok(&self) -> bool {
        self.status >= 200 && self.status <= 299
    }

    pub fn redirect(&self) -> bool {
        self.status >= 300 && self.status <= 399
    }

    /// Whether the response status is: 400 <= status <= 499
    pub fn client_error(&self) -> bool {
        self.status >= 400 && self.status <= 499
    }

    /// Whether the response status is: 500 <= status <= 599
    pub fn server_error(&self) -> bool {
        self.status >= 500 && self.status <= 599
    }

    /// Whether the response status is: 400 <= status <= 599
    pub fn error(&self) -> bool {
        self.client_error() || self.server_error()
    }

    /// Tells if this response is "synthetic".
    ///
    /// The [methods](struct.Request.html#method.call) [firing](struct.Request.html#method.send)
    /// [off](struct.Request.html#method.send_str) [requests](struct.Request.html#method.send_json)
    /// all return a `Response`; there is no rust style `Result`.
    ///
    /// Rather than exposing a custom error type through results, this library has opted
    /// for representing potential connection/TLS/etc errors as HTTP response codes.
    /// These invented codes are called "synthetic".
    ///
    /// The idea is that from a library user's point of view the distinction
    /// of whether a failure originated in the remote server (500, 502) etc, or some transient
    /// network failure, the code path of handling that would most often be the same.
    ///
    /// The specific mapping of error to code can be seen in the [`Error`](enum.Error.html) doc.
    ///
    /// However if the distinction is important, this method can be used to tell. Also see
    /// [error()](struct.Response.html#method.synthetic_error) to see the actual underlying error.
    ///
    /// ```
    /// // scheme that this library doesn't understand
    /// let resp = ureq::get("borkedscheme://www.google.com").call();
    ///
    /// // it's an error
    /// assert!(resp.error());
    ///
    /// // synthetic error code 400
    /// assert_eq!(*resp.status(), 400);
    ///
    /// // tell that it's synthetic.
    /// assert!(resp.synthetic());
    /// ```
    pub fn synthetic(&self) -> bool {
        self.error.is_some()
    }

    /// Get the actual underlying error when the response is
    /// ["synthetic"](struct.Response.html#method.synthetic).
    pub fn synthetic_error(&self) -> &Option<Error> {
        &self.error
    }

    /// The content type part of the "Content-Type" header without
    /// the charset.
    ///
    /// Example:
    ///
    /// ```
    /// let resp = ureq::get("https://www.google.com/").call();
    /// assert_eq!("text/html; charset=ISO-8859-1", resp.header("content-type").unwrap());
    /// assert_eq!("text/html", resp.content_type());
    /// ```
    pub fn content_type(&self) -> &str {
        self.header("content-type")
            .map(|header| {
                header
                    .find(";")
                    .map(|index| &header[0..index])
                    .unwrap_or(header)
            })
            .unwrap_or(DEFAULT_CONTENT_TYPE)
    }

    /// The character set part of the "Content-Type" header.native_tls
    ///
    /// Example:
    ///
    /// ```
    /// let resp = ureq::get("https://www.google.com/").call();
    /// assert_eq!("text/html; charset=ISO-8859-1", resp.header("content-type").unwrap());
    /// assert_eq!("ISO-8859-1", resp.charset());
    /// ```
    pub fn charset(&self) -> &str {
        self.header("content-type")
            .and_then(|header| {
                header.find(";").and_then(|semi| {
                    (&header[semi + 1..])
                        .find("=")
                        .map(|equal| (&header[semi + equal + 2..]).trim())
                })
            })
            .unwrap_or(DEFAULT_CHARACTER_SET)
    }

    /// Turn this response into a `impl Read` of the body.
    ///
    /// 1. If "Transfer-Encoding: chunked", the returned reader will unchunk it
    ///    and any "Content-Length" header is ignored.
    /// 2. If "Content-Length" is set, the returned reader is limited to this byte
    ///    length regardless of how many bytes the server sends.
    /// 3. If no length header, the reader is until server stream end.
    ///
    /// Example:
    ///
    /// ```
    /// use std::io::Read;
    ///
    /// let resp =
    ///     ureq::get("https://raw.githubusercontent.com/algesten/ureq/master/.gitignore").call();
    ///
    /// assert!(resp.has("Content-Length"));
    /// let len = resp.header("Content-Length")
    ///     .and_then(|s| s.parse::<usize>().ok()).unwrap();
    ///
    /// let mut reader = resp.into_reader();
    /// let mut bytes = vec![];
    /// reader.read_to_end(&mut bytes);
    ///
    /// assert_eq!(bytes.len(), len);
    /// ```
    pub fn into_reader(self) -> impl Read {
        let is_chunked = self.header("transfer-encoding")
            .map(|enc| enc.len() > 0) // whatever it says, do chunked
            .unwrap_or(false);
        let len = self.header("content-length")
            .and_then(|l| l.parse::<usize>().ok());
        let reader = self.stream.expect("No reader in response?!");
        match is_chunked {
            true => Box::new(chunked_transfer::Decoder::new(reader)),
            false => match len {
                Some(len) => Box::new(LimitedRead::new(reader, len)),
                None => Box::new(reader) as Box<Read>,
            },
        }
    }

    /// Turn this response into a String of the response body. Attempts to respect the
    /// character encoding of the "Content-Type" and falls back to `utf-8`.
    ///
    /// This is potentially memory inefficient for large bodies since the
    /// implementation first reads the reader to end into a `Vec<u8>` and then
    /// attempts to decode it using the charset.
    ///
    /// Example:
    ///
    /// ```
    /// let resp =
    ///     ureq::get("https://raw.githubusercontent.com/algesten/ureq/master/.gitignore").call();
    ///
    /// let text = resp.into_string().unwrap();
    ///
    /// assert!(text.contains("target"));
    /// ```
    pub fn into_string(self) -> IoResult<String> {
        let encoding = encoding_from_whatwg_label(self.charset())
            .or_else(|| encoding_from_whatwg_label(DEFAULT_CHARACTER_SET))
            .unwrap();
        let mut buf: Vec<u8> = vec![];
        self.into_reader().read_to_end(&mut buf)?;
        Ok(encoding.decode(&buf, DecoderTrap::Replace).unwrap())
    }

    /// Turn this response into a (serde) JSON value of the response body.
    ///
    /// Example:
    ///
    /// ```
    /// let resp =
    ///     ureq::get("https://raw.githubusercontent.com/algesten/ureq/master/src/test/hello_world.json").call();
    ///
    /// let json = resp.into_json().unwrap();
    ///
    /// assert_eq!(json["hello"], "world");
    /// ```
    pub fn into_json(self) -> IoResult<serde_json::Value> {
        let reader = self.into_reader();
        serde_json::from_reader(reader).map_err(|e| {
            IoError::new(
                ErrorKind::InvalidData,
                format!("Failed to read JSON: {}", e),
            )
        })
    }

    /// Create a response from a Read trait impl.
    ///
    /// This is hopefully useful for unit tests.
    ///
    /// Example:
    ///
    /// ```
    /// use std::io::Cursor;
    ///
    /// let text = "HTTP/1.1 401 Authorization Required\r\n\r\nPlease log in\n";
    /// let read = Cursor::new(text.to_string().into_bytes());
    /// let resp = ureq::Response::from_read(read);
    ///
    /// assert_eq!(*resp.status(), 401);
    /// ```
    pub fn from_read(reader: impl Read) -> Self {
        Self::do_from_read(reader).unwrap_or_else(|e| e.into())
    }

    fn do_from_read(mut reader: impl Read) -> Result<Response, Error> {
        //
        // HTTP/1.1 200 OK\r\n
        let status_line = read_next_line(&mut reader).map_err(|_| Error::BadStatus)?;

        let (index, status) = parse_status_line(status_line.as_str())?;

        let mut headers: Vec<Header> = Vec::new();
        loop {
            let line = read_next_line(&mut reader).map_err(|_| Error::BadHeader)?;
            if line.len() == 0 {
                break;
            }
            if let Ok(header) = line.as_str().parse::<Header>() {
                headers.push(header);
            }
        }

        Ok(Response {
            error: None,
            status_line,
            index,
            status,
            headers,
            stream: None,
        })
    }

    fn set_stream(&mut self, stream: Stream) {
        self.stream = Some(stream);
    }

    #[cfg(test)]
    pub fn to_write_vec(&self) -> Vec<u8> {
        self.stream.as_ref().unwrap().to_write_vec()
    }
}

fn parse_status_line(line: &str) -> Result<((usize, usize), u16), Error> {
    // HTTP/1.1 200 OK\r\n
    let mut split = line.splitn(3, ' ');

    let http_version = split.next().ok_or_else(|| Error::BadStatus)?;
    if http_version.len() < 5 {
        return Err(Error::BadStatus);
    }
    let index1 = http_version.len();

    let status = split.next().ok_or_else(|| Error::BadStatus)?;
    if status.len() < 3 {
        return Err(Error::BadStatus);
    }
    let index2 = index1 + status.len();

    let status = status.parse::<u16>().map_err(|_| Error::BadStatus)?;

    let status_text = split.next().ok_or_else(|| Error::BadStatus)?;
    if status_text.len() == 0 {
        return Err(Error::BadStatus);
    }

    Ok(((index1, index2), status))
}

impl FromStr for Response {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = s.as_bytes().to_owned();
        let mut cursor = Cursor::new(bytes);
        let mut resp = Self::do_from_read(&mut cursor)?;
        resp.set_stream(Stream::new(StreamImp::Cursor(cursor)));
        Ok(resp)
    }
}

impl Into<Response> for Error {
    fn into(self) -> Response {
        let status = self.status();
        let status_text = self.status_text().to_string();
        let body_text = self.body_text();
        let mut resp = Response::new(status, &status_text, &body_text);
        resp.error = Some(self);
        resp
    }
}

// application/x-www-form-urlencoded, application/json, and multipart/form-data

fn read_next_line<R: Read>(reader: &mut R) -> IoResult<AsciiString> {
    let mut buf = Vec::new();
    let mut prev_byte_was_cr = false;

    loop {
        let byte = reader.bytes().next();

        let byte = match byte {
            Some(b) => try!(b),
            None => return Err(IoError::new(ErrorKind::ConnectionAborted, "Unexpected EOF")),
        };

        if byte == b'\n' && prev_byte_was_cr {
            buf.pop(); // removing the '\r'
            return AsciiString::from_ascii(buf)
                .map_err(|_| IoError::new(ErrorKind::InvalidInput, "Header is not in ASCII"));
        }

        prev_byte_was_cr = byte == b'\r';

        buf.push(byte);
    }
}

struct LimitedRead {
    reader: Stream,
    limit: usize,
    position: usize,
}

impl LimitedRead {
    fn new(reader: Stream, limit: usize) -> Self {
        LimitedRead {
            reader,
            limit,
            position: 0,
        }
    }
}

impl Read for LimitedRead {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        let left = self.limit - self.position;
        let from = if left < buf.len() {
            &mut buf[0..left]
        } else {
            buf
        };
        match self.reader.read(from) {
            Ok(amount) => {
                self.position += amount;
                Ok(amount)
            }
            Err(e) => Err(e),
        }
    }
}
