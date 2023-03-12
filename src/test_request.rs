use ::anyhow::anyhow;
use ::anyhow::Context;
use ::anyhow::Result;
use ::auto_future::AutoFuture;
use ::axum::http::HeaderValue;
use ::cookie::Cookie;
use ::cookie::CookieJar;
use ::hyper::body::to_bytes;
use ::hyper::body::Body;
use ::hyper::body::Bytes;
use ::hyper::header;
use ::hyper::header::HeaderName;
use ::hyper::http::header::SET_COOKIE;
use ::hyper::http::Request;
use ::hyper::Client;
use ::serde::Serialize;
use ::serde_json::to_vec as json_to_vec;
use ::std::convert::AsRef;
use ::std::fmt::Debug;
use ::std::fmt::Display;
use ::std::future::IntoFuture;
use ::std::sync::Arc;
use ::std::sync::Mutex;

use crate::InnerTestServer;
use crate::TestResponse;

mod test_request_config;
pub(crate) use self::test_request_config::*;

mod test_request_details;
pub(crate) use self::test_request_details::*;

const JSON_CONTENT_TYPE: &'static str = &"application/json";
const TEXT_CONTENT_TYPE: &'static str = &"text/plain";

///
/// A `TestRequest` represents a HTTP request to the test server.
///
/// ## Creating
///
/// Requests are created by the `TestServer`. You do not create them yourself.
///
/// The `TestServer` has functions corresponding to specific requests.
/// For example calling `TestServer::get` to create a new HTTP GET request,
/// or `TestServer::post to create a HTTP POST request.
///
/// ## Customising
///
/// The `TestRequest` allows the caller to fill in the rest of the request
/// to be sent to the server. Including the headers, the body, cookies, the content type,
/// and other relevant details.
///
/// The TestRequest struct provides a number of methods to set up the request,
/// such as json, text, bytes, expect_failure, content_type, etc.
/// The do_save_cookies and do_not_save_cookies methods are used to control cookie handling.
///
/// ## Sending
///
/// Once fully configured you send the rquest by awaiting the request object.
///
/// ```rust,ignore
/// let request = server.get(&"/user");
/// let response = request.await;
/// ```
///
/// You will receive back a `TestResponse`.
///
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct TestRequest {
    details: TestRequestDetails,

    inner_test_server: Arc<Mutex<InnerTestServer>>,

    full_request_path: String,
    body: Option<Body>,
    headers: Vec<(HeaderName, HeaderValue)>,
    cookies: CookieJar,
    content_type: Option<String>,

    is_expecting_failure: bool,
    is_saving_cookies: bool,
}

impl TestRequest {
    pub(crate) fn new(
        inner_test_server: Arc<Mutex<InnerTestServer>>,
        config: TestRequestConfig,
        details: TestRequestDetails,
    ) -> Result<Self> {
        let server_locked = inner_test_server.as_ref().lock().map_err(|err| {
            anyhow!(
                "Failed to lock InternalTestServer for {} {}, received {:?}",
                details.method,
                details.path,
                err
            )
        })?;
        let full_request_path = build_request_path(server_locked.server_address(), &details.path);

        let cookies = server_locked.cookies().clone();

        ::std::mem::drop(server_locked);

        Ok(Self {
            details,
            inner_test_server,
            full_request_path,
            body: None,
            headers: vec![],
            cookies,
            content_type: config.content_type,
            is_expecting_failure: false,
            is_saving_cookies: config.save_cookies,
        })
    }

    /// Any cookies returned will be saved to the `TestServer` that created this,
    /// which will continue to use those cookies on future requests.
    pub fn do_save_cookies(mut self) -> Self {
        self.is_saving_cookies = true;
        self
    }

    /// Cookies returned by this will _not_ be saved to the `TestServer`.
    /// For use by future requests.
    ///
    /// This is the default behaviour.
    /// You can change that default in `TestServerConfig`.
    pub fn do_not_save_cookies(mut self) -> Self {
        self.is_saving_cookies = false;
        self
    }

    /// Clears all cookies used internally within this Request.
    pub fn clear_cookies(mut self) -> Self {
        self.cookies = CookieJar::new();
        self
    }

    /// Adds a Cookie to be sent with this request.
    pub fn add_cookie<'c>(mut self, cookie: Cookie<'c>) -> Self {
        self.cookies.add(cookie.into_owned());
        self
    }

    /// Marks that this request should expect to fail.
    /// Failiure is deemend as any response that isn't a 200.
    ///
    /// By default, requests are expct to always succeed.
    pub fn expect_failure(mut self) -> Self {
        self.is_expecting_failure = true;
        self
    }

    /// Marks that this request should expect to succeed.
    /// Success is deemend as returning a 200.
    ///
    /// Note this is the default behaviour when creating a new `TestRequest`.
    pub fn expect_success(mut self) -> Self {
        self.is_expecting_failure = false;
        self
    }

    /// Set the body of the request to send up as Json.
    pub fn json<J>(mut self, body: &J) -> Self
    where
        J: ?Sized + Serialize,
    {
        let body_bytes = json_to_vec(body).expect("It should serialize the content into JSON");
        let body: Body = body_bytes.into();
        self.body = Some(body);

        if self.content_type == None {
            self.content_type = Some(JSON_CONTENT_TYPE.to_string());
        }

        self
    }

    /// Set raw text as the body of the request.
    ///
    /// If there isn't a content type set, this will default to `text/plain`.
    pub fn text<T>(mut self, raw_text: T) -> Self
    where
        T: Display,
    {
        let body_text = format!("{}", raw_text);
        let body_bytes = Bytes::from(body_text.into_bytes());

        if self.content_type == None {
            self.content_type = Some(TEXT_CONTENT_TYPE.to_string());
        }

        self.bytes(body_bytes)
    }

    /// Set raw bytes as the body of the request.
    ///
    /// The content type is left unchanged.
    pub fn bytes(mut self, body_bytes: Bytes) -> Self {
        let body: Body = body_bytes.into();

        self.body = Some(body);
        self
    }

    /// Set the content type to use for this request in the header.
    pub fn content_type(mut self, content_type: &str) -> Self {
        self.content_type = Some(content_type.to_string());
        self
    }

    async fn send_or_panic(self) -> TestResponse {
        self.send().await.expect("Sending request failed")
    }

    async fn send(mut self) -> Result<TestResponse> {
        let path = self.details.path;
        let save_cookies = self.is_saving_cookies;
        let body = self.body.unwrap_or(Body::empty());

        let mut request_builder = Request::builder()
            .uri(&self.full_request_path)
            .method(self.details.method);

        // Add all the headers we have.
        let mut headers = self.headers;
        if let Some(content_type) = self.content_type {
            let header = build_content_type_header(content_type)?;
            headers.push(header);
        }

        // Add all the cookies as headers
        for cookie in self.cookies.iter() {
            let cookie_raw = cookie.to_string();
            let header_value = HeaderValue::from_str(&cookie_raw)?;
            headers.push((header::COOKIE, header_value));
        }

        // Put headers into the request
        for (header_name, header_value) in headers {
            request_builder = request_builder.header(header_name, header_value);
        }

        let request = request_builder.body(body).with_context(|| {
            format!(
                "Expect valid hyper Request to be built on request to {}",
                path
            )
        })?;

        let hyper_response = Client::new()
            .request(request)
            .await
            .with_context(|| format!("Expect Hyper Response to succeed on request to {}", path))?;

        let (parts, response_body) = hyper_response.into_parts();
        let response_bytes = to_bytes(response_body).await?;

        if save_cookies {
            let cookie_headers = parts.headers.get_all(SET_COOKIE).into_iter();
            InnerTestServer::add_cookies_by_header(&mut self.inner_test_server, cookie_headers)?;
        }

        let mut response = TestResponse::new(path, parts, response_bytes);

        // Assert if ok or not.
        if self.is_expecting_failure {
            response = response.assert_status_not_ok();
        } else {
            response = response.assert_status_ok();
        }

        Ok(response)
    }
}

impl IntoFuture for TestRequest {
    type Output = TestResponse;
    type IntoFuture = AutoFuture<TestResponse>;

    fn into_future(self) -> Self::IntoFuture {
        let raw_future = self.send_or_panic();
        AutoFuture::new(raw_future)
    }
}

fn build_request_path(root_path: &str, sub_path: &str) -> String {
    if sub_path == "" {
        return format!("http://{}", root_path.to_string());
    }

    if sub_path.starts_with("/") {
        return format!("http://{}{}", root_path, sub_path);
    }

    format!("http://{}/{}", root_path, sub_path)
}

fn build_content_type_header(content_type: String) -> Result<(HeaderName, HeaderValue)> {
    let header_value = HeaderValue::from_str(&content_type)
        .with_context(|| format!("Failed to store header content type '{}'", content_type))?;

    Ok((header::CONTENT_TYPE, header_value))
}
