//! Network input (#15): pulls a WAV or LPCM stream from a URL into a Virtual input's channel sink.
//! U1 skeleton: signatures only. The bodies arrive in U3.
use crate::config::Config;
use crate::pw::Graph;
use std::io::Read;

#[derive(Clone, Debug, PartialEq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
    pub big_endian: bool,
    pub header_len: usize,
}

#[derive(Default)]
pub struct In {}

impl In {
    pub fn tick(&mut self, _cfg: &Config, _g: &Graph) {}
}

pub fn valid_url(_url: &str) -> bool {
    todo!("#15 U1 stub")
}

pub fn cli(_args: &[String]) -> Option<i32> {
    None
}

fn sniff(_content_type: Option<&str>, _head: &[u8]) -> Result<Format, String> {
    todo!("#15 U1 stub")
}

fn to_le(_buf: &mut [u8]) {
    todo!("#15 U1 stub")
}

fn play_args(_id: &str, _f: &Format) -> Vec<String> {
    todo!("#15 U1 stub")
}

fn wanted(_cfg: &Config, _present: &dyn Fn(&str) -> bool) -> Vec<(String, String)> {
    todo!("#15 U1 stub")
}

fn open(_url: &str) -> Result<(Format, Vec<u8>, Box<dyn Read + Send>), String> {
    todo!("#15 U1 stub")
}

#[cfg(test)]
#[path = "netin_tests.rs"]
mod tests;
