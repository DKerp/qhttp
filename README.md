# QHTTP

The Quantum HTTP (QHTTP) server framework is dedicated to provide a fully functionional HTTP server which enables you to configure all parts of it,
which is key to provide a safe and thus production ready web server.

QHTTP puts safety and reliability first. The goal is to provide a HTTP server that
is resilent against most application level (D)DOS attacks.

This crate is a __work in progress__.

# Motivation

There are already quite a few HTTP frameworks available for Rust, but none of them
allowed me to configure all security relevant limits and timeouts, at least not in
an obvious and idiomatic way.

Before switching to Rust I have worked with [Golang](https://go.dev/). I started with its internal [`net/http`](https://golang.google.cn/pkg/net/http/) library
and have later switched to [FastHTTP](https://github.com/valyala/fasthttp). Especially the latter provides easy ways to apply limits and timeouts. But the Rust libraries are a bit more difficult in that regard.

As an example, the [hyper](https://hyper.rs) framework does not seem to provide a way to limit the size of http headers. I am sure there is a hard coded limit somewhere, but I would like to adjust it. It is also not possible to provide timeouts for the reading and sending of requests headers, at least not in an idiomatic way. In fact I wouldn't know how to implement them for individual HTTP/2 streams.

The [actix](http://actix.rs) framework allows setting quite a lot of timeouts, but as far as I know you cannot adjust the maximum size of headers. It also doesn't seem as if you can apply timeouts for sending intermediate `101 Continue` headers.

I want to have a rust library where I can define _all_ relevant limits and timeouts in a single comprehensive `Config` structure. It should allow me to (rate-)limit all parts of the processing cycle that could be exploited by adversaries in any way imaginable.

So this is the goal of this library: Providing a HTTP server framework which allows me to configure even the most atomic operations, basically on a _quantum level_.

# Status

This crate is __not production ready__. Many of the security features are not yet implemented, expecially for HTTP/2. The server has also not yet undergone extensive testing, not to speak of proper benchmarks to analyse its behavior under heavy load.

The API is __not stabilized yet__. It currently uses a custom `Handler` trait definition, but I will switch over to [`tower`](https://docs.rs/tower)'s service trait definition in the near future so QHTTP can make use of the libraries build upon it.

But the server works in general. It speaks HTTP/1.1 and HTTP/2 and can determine the latter over the TLS APLN extension. Both sending files over GET as well as receiving them over POST requests works for both HTTP versions. The HTTP/2 native rate-limiting is also (seemingly) correctly applied and enforced.
