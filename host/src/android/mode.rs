// The resolution on Android: an app cannot change the one of the screen of the phone - the viewer gets the frames of its size.

use super::video::Display;

pub fn restore_all() {}

pub fn restore_one(_display: &Display) {}

pub fn fit(_display: &Display, _width: u32, _height: u32) {}
