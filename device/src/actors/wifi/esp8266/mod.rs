use crate::drivers::wifi::esp8266::*;
use crate::fmt::*;
use crate::{
    kernel::{actor::Actor, channel::*},
    traits::{
        ip::{IpAddress, IpProtocol, SocketAddress},
        tcp::{TcpError, TcpStack},
        wifi::{Join, JoinError, WifiSupplicant},
    },
};
use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
};
use embassy::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt},
    util::Signal,
};
use embedded_hal::digital::v2::OutputPin;
use futures::future::{select, Either};
use futures::pin_mut;
use heapless::consts::U2;

/// Convenience actor implementation of modem
pub struct Esp8266ModemActor<'a, UART, ENABLE, RESET>
where
    UART: AsyncBufRead + AsyncBufReadExt + AsyncWrite + AsyncWriteExt + 'static,
    ENABLE: OutputPin + 'static,
    RESET: OutputPin + 'static,
{
    modem: Option<Esp8266Modem<'a, UART, ENABLE, RESET>>,
}

impl<'a, UART, ENABLE, RESET> Esp8266ModemActor<'a, UART, ENABLE, RESET>
where
    UART: AsyncBufRead + AsyncBufReadExt + AsyncWrite + AsyncWriteExt + 'static,
    ENABLE: OutputPin + 'static,
    RESET: OutputPin + 'static,
{
    pub fn new() -> Self {
        Self { modem: None }
    }
}

impl<'a, UART, ENABLE, RESET> Unpin for Esp8266ModemActor<'a, UART, ENABLE, RESET>
where
    UART: AsyncBufRead + AsyncBufReadExt + AsyncWrite + AsyncWriteExt + 'static,
    ENABLE: OutputPin + 'static,
    RESET: OutputPin + 'static,
{
}

impl<'a, UART, ENABLE, RESET> Actor for Esp8266ModemActor<'a, UART, ENABLE, RESET>
where
    UART: AsyncBufRead + AsyncBufReadExt + AsyncWrite + AsyncWriteExt + 'static,
    ENABLE: OutputPin + 'static,
    RESET: OutputPin + 'static,
{
    type Configuration = Esp8266Modem<'a, UART, ENABLE, RESET>;
    #[rustfmt::skip]
    type Message<'m> where 'a: 'm = ();

    fn on_mount(&mut self, config: Self::Configuration) {
        self.modem.replace(config);
    }

    #[rustfmt::skip]
    type OnStartFuture<'m> where 'a: 'm = impl Future<Output = ()> + 'm;
    fn on_start(mut self: Pin<&'_ mut Self>) -> Self::OnStartFuture<'_> {
        async move {
            self.modem.as_mut().unwrap().run().await;
        }
    }

    #[rustfmt::skip]
    type OnMessageFuture<'m> where 'a: 'm = impl Future<Output = ()> + 'm;
    fn on_message<'m>(self: Pin<&'m mut Self>, _: Self::Message<'m>) -> Self::OnMessageFuture<'m> {
        async move {}
    }
}
