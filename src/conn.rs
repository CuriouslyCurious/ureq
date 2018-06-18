use std::io::Write;
use url::Url;

const CHUNK_SIZE: usize = 1024 * 1024;

fn connect(
    request: &Request,
    method: &str,
    url: &Url,
    redirects: u32,
    body: SizedReader,
    agent: Arc<Mutex<Option<AgentState>>>,
) -> Result<Response, Error> {
    //

    let do_chunk = request.header("transfer-encoding")
            // if the user has set an encoding header, obey that.
            .map(|enc| enc.len() > 0)
            // otherwise, no chunking.
            .unwrap_or(false);

    let hostname = url.host_str().unwrap_or("localhost"); // is localhost a good alternative?

    let query_string = match (url.query(), request.query.len() > 0) {
        (Some(urlq), true) => format!("?{}{}", urlq, request.query),
        (Some(urlq), false) => format!("?{}", urlq),
        (None, true) => format!("?{}", request.query),
        (None, false) => "".to_string(),
    };

    let is_secure = url.scheme().eq_ignore_ascii_case("https");

    let cookie_headers: Vec<_> = {
        let state = agent.lock().unwrap();
        state
            .as_ref()
            .map(|s| &s.jar)
            .map(|jar| match_cookies(jar, hostname, url.path(), is_secure))
            .unwrap_or_else(|| vec![])
    };
    let extra_headers = {
        let mut extra = vec![];

        // chunking and Content-Length headers are mutually exclusive
        // also don't write this if the user has set it themselves
        if !do_chunk && !request.has("content-length") {
            if let Some(size) = body.size {
                extra.push(format!("Content-Length: {}\r\n", size).parse::<Header>()?);
            }
        }
        extra
    };
    let headers = request
        .headers
        .iter()
        .chain(cookie_headers.iter())
        .chain(extra_headers.iter());

    // attempt to reuse a pooled connection
    let pooled_conn = {
        let mut state = agent.lock().unwrap();
        state
            .as_mut()
            .map(|s| &mut s.pool)
            .and_then(|pool| pool.get_connection(&url))
    };

    let mut stream = if let Some(conn) = pooled_conn {
        conn
    } else {
        // open new socket
        match url.scheme() {
            "http" => connect_http(request, &url),
            "https" => connect_https(request, &url),
            "test" => connect_test(request, &url),
            _ => Err(Error::UnknownScheme(url.scheme().to_string())),
        }?
    };

    // send the request start + headers
    let mut prelude: Vec<u8> = vec![];
    write!(
        prelude,
        "{} {}{} HTTP/1.1\r\n",
        method,
        url.path(),
        query_string
    )?;
    if !request.has("host") {
        write!(prelude, "Host: {}\r\n", url.host().unwrap())?;
    }
    for header in headers {
        write!(prelude, "{}: {}\r\n", header.name(), header.value())?;
    }
    write!(prelude, "\r\n")?;

    stream.write_all(&mut prelude[..])?;

    // start reading the response to process cookies and redirects.
    let mut resp = Response::from_read(&mut stream);

    // squirrel away cookies
    {
        let mut state = agent.lock().unwrap();
        if let Some(state) = state.as_mut() {
            for raw_cookie in resp.all("set-cookie").iter() {
                let to_parse = if raw_cookie.to_lowercase().contains("domain=") {
                    raw_cookie.to_string()
                } else {
                    format!("{}; Domain={}", raw_cookie, hostname)
                };
                match Cookie::parse_encoded(&to_parse[..]) {
                    Err(_) => (), // ignore unparseable cookies
                    Ok(mut cookie) => {
                        let cookie = cookie.into_owned();
                        state.jar.add(cookie)
                    }
                }
            }
        }
    }

    // handle redirects
    if resp.redirect() {
        if redirects == 0 {
            return Err(Error::TooManyRedirects);
        }

        // the location header
        let location = resp.header("location");
        if let Some(location) = location {
            // join location header to current url in case it it relative
            let new_url = url.join(location)
                .map_err(|_| Error::BadUrl(format!("Bad redirection: {}", location)))?;

            // perform the redirect differently depending on 3xx code.
            return match resp.status {
                301 | 302 | 303 => {
                    send_body(body, do_chunk, &mut stream)?;
                    let empty = Payload::Empty.into_read();
                    connect(request, "GET", &new_url, redirects - 1, empty, agent)
                }
                307 | 308 | _ => connect(request, method, &new_url, redirects - 1, body, agent),
            };
        }
    }

    // send the body (which can be empty now depending on redirects)
    send_body(body, do_chunk, &mut stream)?;

    // did user set connection close to force it?
    let req_is_close = request
            .header("connection")
            .map(|k| k.to_lowercase().contains("close"))
            .unwrap_or(false);

    // or did server respond with connection close?
    let resp_is_close = resp.header("connection")
        .map(|k| k.to_lowercase().contains("close"))
        .unwrap_or(false);

    let keep_alive_possible = !req_is_close && !resp_is_close
        && (resp.has("content-length") || resp.has("transfer-encoding"));

    // since it is not a redirect, give away the incoming stream to the response object
    resp.set_keep_alive(KeepAlive {
        stream,
        url: url.clone(),
        agent: if keep_alive_possible {
            Some(agent)
        } else {
            None
        },
    });

    // release the response
    Ok(resp)
}

fn send_body(body: SizedReader, do_chunk: bool, stream: &mut Stream) -> IoResult<()> {
    if do_chunk {
        pipe(body.reader, chunked_transfer::Encoder::new(stream))?;
    } else {
        pipe(body.reader, stream)?;
    }

    Ok(())
}

fn pipe<R, W>(mut reader: R, mut writer: W) -> IoResult<()>
where
    R: Read,
    W: Write,
{
    let mut buf = [0_u8; CHUNK_SIZE];
    loop {
        let len = reader.read(&mut buf)?;
        if len == 0 {
            break;
        }
        writer.write_all(&buf[0..len])?;
    }
    Ok(())
}

// TODO check so cookies can't be set for tld:s
fn match_cookies<'a>(jar: &'a CookieJar, domain: &str, path: &str, is_secure: bool) -> Vec<Header> {
    jar.iter()
        .filter(|c| {
            // if there is a domain, it must be matched. if there is no domain, then ignore cookie
            let domain_ok = c.domain()
                .map(|cdom| domain.contains(cdom))
                .unwrap_or(false);
            // a path must match the beginning of request path. no cookie path, we say is ok. is it?!
            let path_ok = c.path()
                .map(|cpath| path.find(cpath).map(|pos| pos == 0).unwrap_or(false))
                .unwrap_or(true);
            // either the cookie isnt secure, or we're not doing a secure request.
            let secure_ok = !c.secure() || is_secure;

            domain_ok && path_ok && secure_ok
        })
        .map(|c| {
            let name = c.name().to_string();
            let value = c.value().to_string();
            let nameval = Cookie::new(name, value).encoded().to_string();
            let head = format!("Cookie: {}", nameval);
            head.parse::<Header>().ok()
        })
        .filter(|o| o.is_some())
        .map(|o| o.unwrap())
        .collect()
}
