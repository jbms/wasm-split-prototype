use std::pin::Pin;

use futures::io::AsyncReadExt;
use wasm_bindgen::prelude::*;
use wasm_split::wasm_split;
use wasm_streams::ReadableStream;

#[cfg_attr(feature = "split", wasm_split::wasm_split(gzip))]
async fn get_gzip_decoder(
    encoded_reader: Pin<Box<dyn futures::io::AsyncBufRead>>,
) -> Pin<Box<dyn futures::io::AsyncRead>> {
    Box::pin(async_compression::futures::bufread::GzipDecoder::new(
        encoded_reader,
    ))
}

#[cfg_attr(feature = "split", wasm_split(brotli))]
async fn get_brotli_decoder(
    encoded_reader: Pin<Box<dyn futures::io::AsyncBufRead>>,
) -> Pin<Box<dyn futures::io::AsyncRead>> {
    Box::pin(async_compression::futures::bufread::BrotliDecoder::new(
        encoded_reader,
    ))
}

#[wasm_bindgen]
pub async fn decode(url: &str) -> Result<String, JsError> {
    let response = gloo_net::http::Request::get(url).send().await?;
    if response.status() != 200 {
        return Err(JsError::new(
            format!(
                "Received HTTP error {}: {}",
                response.status(),
                response.status_text()
            )
            .as_str(),
        ));
    }
    let body = ReadableStream::from_raw(response.body().unwrap_throw()).into_async_read();
    let buf_raw = Box::pin(futures::io::BufReader::new(body));
    let mut decoded = if url.ends_with(".gz") {
        get_gzip_decoder(buf_raw).await
    } else if url.ends_with(".br") {
        get_brotli_decoder(buf_raw).await
    } else {
        buf_raw
    };
    let mut data = Vec::new();
    decoded.read_to_end(&mut data).await?;
    Ok(String::from_utf8(data)?)
}
