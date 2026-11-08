//! Network output (#15, SWYH-style): a Network bus plays on a DLNA renderer and/or is served over HTTP.
//! U1 skeleton: signatures only. The bodies arrive in U2.
use crate::config::{Config, Renderer};
use crate::pw::Graph;
use std::net::{IpAddr, TcpListener};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PORT: u16 = 5959;

#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub udn: String,
    pub name: String,
    pub location: String,
    pub control: String,
}

#[derive(Default)]
pub struct Out {}

impl Out {
    pub fn tick(&mut self, _cfg: &Config, _g: &Graph) {}
}

pub fn discover(_wait: Duration) -> Vec<Found> {
    todo!("#15 U1 stub")
}

pub fn stream_url(_bus_id: &str, _toward: Option<&str>) -> String {
    todo!("#15 U1 stub")
}

pub fn cli(_args: &[String]) -> Option<i32> {
    None
}

fn url_at(_id: &str, _ip: IpAddr) -> String {
    todo!("#15 U1 stub")
}

fn local_ip_for(_host: &str) -> Option<IpAddr> {
    todo!("#15 U1 stub")
}

fn parse_ssdp(_resp: &str) -> Option<String> {
    todo!("#15 U1 stub")
}

fn parse_description(_xml: &str, _location: &str) -> Option<Found> {
    todo!("#15 U1 stub")
}

fn join_url(_base: &str, _rel: &str) -> String {
    todo!("#15 U1 stub")
}

fn esc(_s: &str) -> String {
    todo!("#15 U1 stub")
}

fn envelope(_action: &str, _args: &str) -> String {
    todo!("#15 U1 stub")
}

fn soap_set_uri(_url: &str, _title: &str) -> String {
    todo!("#15 U1 stub")
}

fn wav_header(_rate: u32, _channels: u16) -> [u8; 44] {
    todo!("#15 U1 stub")
}

fn parse_request(_head: &str) -> Option<(String, String)> {
    todo!("#15 U1 stub")
}

fn route(_path: &str, _allowed: &[String]) -> Option<String> {
    todo!("#15 U1 stub")
}

fn tap_args(_bus_id: &str, _n: usize) -> Vec<String> {
    todo!("#15 U1 stub")
}

fn allowed(_cfg: &Config, _present: &dyn Fn(&str) -> bool) -> Vec<(String, String)> {
    todo!("#15 U1 stub")
}

fn wanted_pushes(_cfg: &Config, _present: &dyn Fn(&str) -> bool) -> Vec<(String, Renderer)> {
    todo!("#15 U1 stub")
}

fn serve(_l: TcpListener, _allowed: Arc<Mutex<Vec<(String, String)>>>, _stop: Arc<AtomicBool>) {
    todo!("#15 U1 stub")
}

#[cfg(test)]
#[path = "swyh_tests.rs"]
mod tests;
