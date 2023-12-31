# Pocket Realy Client Shared

![License](https://img.shields.io/github/license/PocketRelay/PocketRelayClientShared?style=for-the-badge)
![Build](https://img.shields.io/github/actions/workflow/status/PocketRelay/PocketRelayClientShared/build.yml?style=for-the-badge)
![Cargo Version](https://img.shields.io/crates/v/pocket-relay-client-shared?style=for-the-badge)
![Cargo Downloads](https://img.shields.io/crates/d/pocket-relay-client-shared?style=for-the-badge)

[Discord Server (discord.gg/yvycWW8RgR)](https://discord.gg/yvycWW8RgR)
[Website (pocket-relay.pages.dev)](https://pocket-relay.pages.dev/)

This is a shared backend implementation for the Pocket Relay client variants so that they can share behavior without creating duplicated code and to make changes more easy to carry across between implementations

```toml
[dependencies]
pocket-relay-client-shared = "0.2"
```

## Used by

This shared backend is used by the following Pocket Relay projects:
- Standalone Client - https://github.com/PocketRelay/Client
  - This is a standalone executable for the client
- ASI Plugin - https://github.com/PocketRelay/PocketRelayClientPlugin
  - This is a plugin variant of the client loaded by binkw32 plugin loaders

## Functionality

- Fire
  - Very basic implementation of the fire packet framing using for the redirector server implementation
- API
  - Provides functions for working with the portions of the server api that the client uses 
  - Provides functions for upgrading connections with the server
- Local Servers
  - Provides local servers for Redirector, QoS, HTTP, Blaze, Telemetry 
- Update
  - Small functions that help with getting update details from github releases
- Tunneling
  - Provides socket pools and tunneling for https://github.com/PocketRelay/Server/issues/64