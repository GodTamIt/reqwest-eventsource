use crate::error::{CannotCloneRequestError, Error};
use crate::retry::{RetryPolicy, DEFAULT_RETRY};
use core::pin::Pin;
use eventsource_stream::Eventsource;
pub use eventsource_stream::{Event as MessageEvent, EventStreamError};
use futures_core::future::{BoxFuture, Future};
use futures_core::stream::{BoxStream, Stream};
use futures_core::task::{Context, Poll};
use futures_timer::Delay;
use pin_project_lite::pin_project;
use reqwest::header::HeaderValue;
use reqwest::Error as ReqwestError;
use reqwest::IntoUrl;
use reqwest::StatusCode;
use reqwest::{RequestBuilder, Response};
use std::time::Duration;

type ResponseFuture = BoxFuture<'static, Result<Response, ReqwestError>>;
type EventStream = BoxStream<'static, Result<MessageEvent, EventStreamError<ReqwestError>>>;
type BoxedRetry = Box<dyn RetryPolicy + Send + Unpin + 'static>;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
#[repr(u8)]
pub enum ReadyState {
    Connecting = 0,
    Open = 1,
    Closed = 2,
}

pin_project! {
/// Provides the [`Stream`] implementation for the [`Event`] items. This wraps the
/// [`RequestBuilder`] and retries requests when they fail.
#[project = EventSourceProjection]
pub struct EventSource {
    builder: RequestBuilder,
    #[pin]
    next_response: Option<ResponseFuture>,
    #[pin]
    cur_stream: Option<EventStream>,
    #[pin]
    delay: Option<Delay>,
    is_closed: bool,
    retry_policy: BoxedRetry,
    last_retry: Option<(usize, Duration)>
}
}

impl EventSource {
    /// Wrap a [`RequestBuilder`]
    pub fn new(builder: RequestBuilder) -> Result<Self, CannotCloneRequestError> {
        let res_future = Box::pin(builder.try_clone().ok_or(CannotCloneRequestError)?.send());
        Ok(Self {
            builder,
            next_response: Some(res_future),
            cur_stream: None,
            delay: None,
            is_closed: false,
            retry_policy: Box::new(DEFAULT_RETRY),
            last_retry: None,
        })
    }

    pub fn get<T: IntoUrl>(url: T) -> Self {
        Self::new(reqwest::Client::new().get(url)).unwrap()
    }

    pub fn close(&mut self) {
        self.is_closed = true;
    }

    pub fn ready_state(&self) -> ReadyState {
        if self.is_closed {
            ReadyState::Closed
        } else if self.delay.is_some() || self.next_response.is_some() {
            ReadyState::Connecting
        } else {
            ReadyState::Open
        }
    }
}

impl<'a> EventSourceProjection<'a> {
    pub fn clear_fetch(&mut self) {
        self.next_response.take();
        self.cur_stream.take();
    }

    pub fn retry_fetch(&mut self) {
        self.cur_stream.take();
        let res_future = Box::pin(self.builder.try_clone().unwrap().send());
        self.next_response.replace(res_future);
    }

    pub fn handle_error(&mut self, error: &Error) {
        self.clear_fetch();
        if let Some(retry_delay) = self.retry_policy.retry(error, *self.last_retry) {
            let retry_num = self.last_retry.map(|retry| retry.0).unwrap_or(1);
            *self.last_retry = Some((retry_num, retry_delay));
            self.delay.replace(Delay::new(retry_delay));
        } else {
            *self.is_closed = true;
        }
    }
}

fn check_response(response: &Response) -> Result<(), Error> {
    if !matches!(response.status(), StatusCode::OK) {
        return Err(Error::InvalidStatusCode(response.status()));
    }
    let content_type = response
        .headers()
        .get(&reqwest::header::CONTENT_TYPE)
        .ok_or(Error::InvalidContentType(HeaderValue::from_static("")))?;
    let mime_type: mime::Mime = content_type
        .to_str()
        .map_err(|_| Error::InvalidContentType(content_type.clone()))?
        .parse()
        .map_err(|_| Error::InvalidContentType(content_type.clone()))?;
    if !matches!(
        (mime_type.type_(), mime_type.subtype()),
        (mime::TEXT, mime::EVENT_STREAM)
    ) {
        return Err(Error::InvalidContentType(content_type.clone()));
    }
    Ok(())
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Event {
    Open,
    Message(MessageEvent),
}

impl From<MessageEvent> for Event {
    fn from(event: MessageEvent) -> Self {
        Event::Message(event)
    }
}

impl Stream for EventSource {
    type Item = Result<Event, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        if *this.is_closed {
            return Poll::Ready(None);
        }

        if let Some(delay) = this.delay.as_mut().as_pin_mut() {
            match delay.poll(cx) {
                Poll::Ready(_) => {
                    this.delay.take();
                    this.retry_fetch();
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        if let Some(response_future) = this.next_response.as_mut().as_pin_mut() {
            match response_future.poll(cx) {
                Poll::Ready(Ok(res)) => {
                    this.clear_fetch();
                    if let Err(err) = check_response(&res) {
                        *this.is_closed = true;
                        return Poll::Ready(Some(Err(err)));
                    }
                    this.last_retry.take();
                    this.cur_stream
                        .replace(Box::pin(res.bytes_stream().eventsource()));
                    return Poll::Ready(Some(Ok(Event::Open)));
                }
                Poll::Ready(Err(err)) => {
                    let err = Error::Transport(err);
                    this.handle_error(&err);
                    return Poll::Ready(Some(Err(err)));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }

        match this
            .cur_stream
            .as_mut()
            .as_pin_mut()
            .unwrap()
            .as_mut()
            .poll_next(cx)
        {
            Poll::Ready(Some(Err(err))) => {
                let err = err.into();
                this.handle_error(&err);
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(Some(Ok(event))) => Poll::Ready(Some(Ok(event.into()))),
            Poll::Ready(None) => {
                let err = Error::StreamEnded;
                this.handle_error(&err);
                Poll::Ready(Some(Err(err)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
