# Upstream provenance

Derived from https://github.com/suenot/vasyaapp at commit `d8e1670` (`vasya-server: accept HttpOnly session cookie + CSRF (M-4 server foundation)`), upstream application version 0.8.0.

The Telegram engine, server, STT and unfinished VoIP helpers, application icon and translation strings were copied under the upstream Unlicense. The new native GUI crates, backend adapters, actor and packaging are maintained together here. Upstream's original checkout is neither modified nor included as a nested Git repository.

The native applications do not compile or ship the upstream React/Tauri frontend. GPUI Kit and Iced keep their own upstream licenses. Packaged FFmpeg and dynamic dependencies retain their separate upstream licenses; `Contents/Resources/ffmpeg-source.txt` records the bundled builds and source locations.
