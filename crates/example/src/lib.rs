use std::pin::Pin;

use futures::io::AsyncReadExt;
use wasm_bindgen::prelude::*;

#[cfg(feature = "split")]
use wasm_split::wasm_split;

use wasm_streams::ReadableStream;

#[cfg(feature = "gzip")]
#[cfg_attr(feature = "split", wasm_split::wasm_split(gzip))]
async fn get_gzip_decoder(
    encoded_reader: Pin<Box<dyn futures::io::AsyncBufRead>>,
) -> Pin<Box<dyn futures::io::AsyncRead>> {
    gloo_console::log!("getting gzip decoder");
    Box::pin(async_compression::futures::bufread::GzipDecoder::new(
        encoded_reader,
    ))
}

#[cfg(feature = "brotli")]
#[cfg_attr(feature = "split", wasm_split(brotli))]
async fn get_brotli_decoder(
    encoded_reader: Pin<Box<dyn futures::io::AsyncBufRead>>,
) -> Pin<Box<dyn futures::io::AsyncRead>> {
    gloo_console::log!("getting brotli decoder");
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
    let buf_raw = Box::pin(body);
    let mut decoded: Pin<Box<dyn futures::io::AsyncRead>> = buf_raw;

    #[cfg(feature = "gzip")]
    if url.ends_with(".gz") {
        decoded = get_gzip_decoder(Box::pin(futures::io::BufReader::new(decoded))).await
    }

    #[cfg(feature = "brotli")]
    if url.ends_with(".br") {
        decoded = get_brotli_decoder(Box::pin(futures::io::BufReader::new(decoded))).await
    }

    let mut data = Vec::new();
    decoded.read_to_end(&mut data).await?;
    Ok(String::from_utf8(data)?)
}
