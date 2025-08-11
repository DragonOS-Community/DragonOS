mod attr;
mod segment;

use crate::net::socket::netlink::{message::Message, route::message::segment::RouteNlSegment};

pub(in crate::net::socket::netlink) type RouteNlMessage = Message<RouteNlSegment>;
