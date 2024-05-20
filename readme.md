# onetime

An async onetime (aka. oneshot) channel, where you can send only one message over that channel.

## Examples
```rust
let (s, r) = onetime::channel();

s.send("ok")?;
let value = r.recv().await?;
```

## License

Licensed under either of
* Apache License, Version 2.0 (LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license (LICENSE-MIT or http://opensource.org/licenses/MIT)

at your option.
