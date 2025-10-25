use http_body_util::{Empty, Full};
use bytes::Bytes;
use hyper::{Response};
use http_body_util::{combinators::BoxBody, BodyExt};

// We create some utility functions to make Empty and Full bodies
// fit our broadened Response body type.

#[inline]
pub fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

#[inline]
pub fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

pub fn response_raw((bytes, ct): (Bytes, Option<String>)) -> Response<BoxBody<Bytes, hyper::Error>> {
    let mut response = Response::new(
        Full::new(bytes)
            .map_err(|never| match never {}).
            boxed()
    );
    if let Some(ct) = ct {
        response.headers_mut().insert(http::header::CONTENT_TYPE, ct.parse().unwrap());
    }
    response
}
