# Upstream provenance

Derived from https://github.com/suenot/vasyaapp at commit `d8e1670` (`vasya-server: accept HttpOnly session cookie + CSRF (M-4 server foundation)`), upstream application version 0.8.0.

The Telegram engine, server, STT and unfinished VoIP helpers, application icon and translation strings were copied under the upstream Unlicense. The new native GUI crates, backend adapters, actor and packaging are maintained together here. Upstream's original checkout is neither modified nor included as a nested Git repository.

The native applications do not compile or ship the upstream React/Tauri frontend. GPUI Kit and Iced keep their own upstream licenses. Packaged FFmpeg and dynamic dependencies retain their separate upstream licenses; `Contents/Resources/ffmpeg-source.txt` records the bundled builds and source locations.

The native 0.10.0 translation service and REST contracts were ported from upstream commit `fd0a8df` (optional bidirectional per-chat LLM translation, upstream application 0.9.0). Native profile isolation, asynchronous controller, virtual lists and prior native engine fixes were preserved.
