# Késésmérés: átadás az új gépre (2026-11-14)

Ezt az ágat (`feature/latency-metrics`) a régi laptopon írta a Claude. Ott nem volt fordító, ezért **semmi nincs belőle
lefordítva, futtatva vagy a Questen kipróbálva**. A tulajdonos döntése: a fordítás és a mérés az új asztali gépen történik,
a meglévő `D:\RustDeskBuild` környezettel.

## Mi van az ágon

- Commit `4c9104ea8` a `feature/microphone-fast-multi` (`e6ed0fcbb`) fölött: csak mérés, a videóút viselkedése, a protokoll,
  az fps, a bitráta és a sorok változatlanok.
- A mérőszámok jelentése, a hibahatárok és a naplók helye: `docs/LATENCY-METRICS.md`.
- Háttér: a Meta Quest Remote Desktop elemzése (miért „azonnali”, mit érdemes átvenni) a privát
  `kdani1/quest-remote-desktop-research` tárolóban van (`gh repo clone kdani1/quest-remote-desktop-research`).
  Kezdd a `README.md`-vel, a fő jelentés a `REPORT.hu.md` (a 8. fejezete felülírja a korábbi állításokat), a PC-oldal a
  `REPORT-PC.hu.md`, a részletek a `notes/` alatt. A mérési eredményt oda, egy `MEASUREMENTS.hu.md` fájlba írd. A javaslatok sorrendje: 0. mérés (ez az ág), 1. rebase az upstreamre és a `hwcodec.rs:239` hibaelnyelés,
  2. eseményvezérelt rögzítés és LAN-profil (60 fps, 10–20 Mbps, alacsony késésű kódoló), 3. „mindig a legújabb kép”
  (`video_ack_required`, 1–2 kép úton), 4. hardveres MediaCodec dekódolás a Questen Surface-be, 5. később UDP/QUIC videó és
  csíkonkénti kódolás.

## Teendők az új gépen, sorrendben

1. `git fetch origin feature/latency-metrics` a fork klónjában (vagy friss klón), és `git submodule update --init`.
2. Nézd meg, mi van a `D:\RustDeskBuild` mappában (VS Build Tools, vcpkg, Rust, Flutter, Android NDK, korábbi build-szkriptek),
   és azzal fordíts. A CI verziói a `.github/workflows/flutter-build.yml` elején vannak (Rust 1.75, Flutter 3.24.5, LLVM 15.0.6,
   NDK r28c, vcpkg `9e593bb18ea69cc5095e012465dcd675a822ed0d`).
3. Fordítsd le a Windows-változatot (gazdagép) és a Quest APK-t (aarch64 Android kliens). A fordítási kockázatokat, amelyeket a
   laptopon nem lehetett ellenőrizni, a commit üzenete és a `LATENCY-METRICS.md` sorolja fel; különösen a `src/flutter.rs`
   `RgbaData.ui_handoff` mezője, a cfg-kapuk és a Dart-oldali `SchedulerBinding` használat. `cargo test` a `src/client/latency.rs`
   egységtesztjeit futtatja.
4. A projekt `AGENTS.md`-je szerint a post-frame callback, a kapcsoló és az időzítő bevezetését a karbantartóval (a tulajdonossal)
   egyeztetni kell: ez az ág ilyeneket ad hozzá, mutasd meg neki.
5. Mérés:
   - A gazdagépen: `rustdesk.exe --option enable-latency-log Y`, a sorok a szerver naplójában (`LATENCY server ...`).
   - A Questen: a Minőségfigyelő bekapcsolása (ott egy `Lat:` sor jelenik meg), vagy a helyi `enable-latency-log` = `Y`;
     `adb logcat | grep LATENCY` (a Rust-sorokat a Dart-oldal is kiírja, mert release buildben a Rust-napló fájlba megy).
   - Ugyanazon a Wi-Fin, ugyanazzal a felbontással mérj: álló kép, egérmozgatás, görgetés, videólejátszás; mindegyikből legalább
     30 s. Jegyezd fel a kodeket (`codec=`), és hogy hardveres-e a dekódolás (`hw=`).
   - Összehasonlításként a Meta Remote Desktop: ugyanazon a gépen és hálózaton, üvegtől üvegig, ms-pontosságú órával a PC
     képernyőjén és Quest-képernyőképpel (vagy lassított kamerás felvétellel); ugyanezzel a módszerrel a RustDesk-et is.
6. Az eredményből döntsd el a következő lépést a fenti sorrend szerint, és ha a mérés mást mutat, mint a becslés, azt írd le.
