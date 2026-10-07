# Android mikrofontovábbítás – kísérleti kiadás

Ez a saját RustDesk fork Android **küldő** változata: a telefon mikrofonját továbbítja a csatlakoztatott Mac/Windows/Linux gép virtuális hangbemenetébe. Mindkét oldalon a mikrofontovábbítást tartalmazó fork szükséges. Más Android-alkalmazások mikrofonbemenetét nem cseréli le.

## Letöltés és telepítés

1. A [GitHub Releases](https://github.com/kdani1/rustdesk/releases) oldalról a `RustDesk-Microphone-Android-arm64.apk` fájlt töltsd le, **ha már megjelent a kiadás mellékletei között**. Az APK elkészítése és aláírása előtt nincs telepíthető kiadás. Az Actions `ci-only-resign-before-distribution.apk` fájlja köztes fordítási eredmény, nem a kiadott APK.
2. Ez ARM64 (`arm64-v8a`) build. A 32 bites Android-rendszerekhez és az x86 emulátorokhoz külön build kell. A készüléken végzett teszt még szükséges.
3. A hivatalos RustDesk más aláírást használ. Ha már telepítve van, előbb jegyezd fel a szerverbeállításokat és mentsd a szükséges alkalmazásadatokat. A hivatalos változatra ez az APK nem telepíthető frissítésként; a régi app eltávolítása az adatait is törölheti. Az új kiadás ugyanazt az alkalmazásazonosítót használja, így egy RustDesk marad a telefonon.
4. Nyisd meg a letöltött APK-t. Ha Android kéri, a letöltéshez használt böngészőnek/fájlkezelőnek engedélyezd az alkalmazástelepítést. Telepítés után ezt az engedélyt kikapcsolhatod.
5. Állítsd be a saját ID-szervert, relay-szervert és a nyilvános szerverkulcsot a privát `Rust-Remote` repó instrukciói szerint. A szerverkulcs nem azonos a célgép állandó jelszavával. Ezek az adatok nem szerepelnek ebben a nyilvános repóban.

## Android → Mac próba

1. A Macen fusson a saját RustDesk változat, legyen telepítve a BlackHole 2ch, és legyen bekapcsolva az **Allow forwarded microphone** beállítás. A képernyőmegosztási és vezérlési engedélyeket külön is rendezni kell.
2. Androidon csatlakozz a Mac RustDesk-azonosítójához, és add meg a Mac állandó hozzáférési jelszavát.
3. Nyomd meg a kapcsolati eszköztár mikrofon gombját (a csevegés menüjéből is elérhető), és engedélyezd a mikrofonhasználatot. Automatikus indításhoz kapcsold be az **Automatically forward microphone** beállítást.
4. Tartsd előtérben a RustDesket a telefonon. A Mac Hang → Bemenet paneljén a BlackHole 2ch legyen a továbbítás idejére kiválasztva, és a telefonba beszélve mozogjon a bemeneti szintmérő.
5. Ezután indítsd el a hangmódot a Macen. Ha a célalkalmazás korábban már megnyitotta a mikrofont, állítsd le és indítsd újra a hangmódot; ha saját eszközválasztója van, válaszd a BlackHole 2ch bemenetet.
6. A mikrofon gombbal állítsd le a továbbítást, majd teszteld a bontást is. A Mac előző bemenete álljon vissza, ha közben nem választottál másikat. A telefon Android mikrofonjelzője tűnjön el.
7. Külön próbáld ki mobilinternetről, kikapcsolt telefonos Wi-Fi mellett. A helyi hálózaton sikeres teszt nem igazolja a külső elérést.

Másik fogadó rendszer virtuális mikrofonjának telepítése: [README.hu.md](README.hu.md). Részletes elfogadási tesztek: [TESTING.md](TESTING.md).

## Aláírás, frissítés és korlátok

A kiadott APK-t helyben megőrzött, saját aláírókulccsal kell aláírni; a kulcs és jelszava nem kerülhet GitHubra vagy Actions artifactba. A következő APK ugyanazzal a kulccsal és növelt verziókóddal frissíthető. A CI ideiglenes aláírását a kiadás előtt le kell cserélni.

A fordítás sikere nem helyettesíti a telefonon végzett hangtesztet. Telefonzár/háttérbe küldés megszakíthatja a rögzítést. A Mac lehajtott fedelével történő alvást ez a funkció nem oldja meg; a fogadó gépnek ébren és hálózaton kell maradnia.

## Build hatóköre

A külön `microphone-android.yml` workflow a meglévő Android Rust/Flutter fordítási útvonalat használja. Az alkalmazás futási kódját nem módosítja; az ideiglenes Gradle memória- és aláírásbeállítás csak a CI munkakönyvtárában változik. A külön buildág nem indítja újra a Windows PR-buildet.

## Helyi fordítás

A saját Android- és Windows-workflow csak kézzel indítható; a normál feltöltés nem indítja ezeket a GitHub gépein. A további fordítás a saját gépeken történik. A korábbi kész Windows-artifactot és Mac-kiadást ez nem törli.

Android ARM64: macOS vagy Linux gépen Flutter 3.24.5, JDK 17, Android SDK 36, NDK r28c, Rust (aarch64-linux-android target), cargo-ndk 3.1.2 és a projekt vcpkg-függőségei szükségesek. Az `ANDROID_NDK_HOME` az NDK könyvtárára mutasson. A generált Flutter-híd elkészítése után a repó gyökeréből a `./flutter/ndk_arm64.sh` fordítja a Rust könyvtárat. Ez a script már az NDK fejléceit adja a bindgennek, és hiányzó NDK esetén egyértelmű hibával leáll. A kész `target/aarch64-linux-android/release/liblibrustdesk.so` fájlt `flutter/android/app/src/main/jniLibs/arm64-v8a/librustdesk.so` néven kell elhelyezni az NDK megfelelő `libc++_shared.so` fájlja mellett, majd a Flutter mappában `flutter build apk --release --target-platform android-arm64 --split-per-abi` futtatható. Kiadáshoz saját Gradle signing config vagy utólagos, megőrzött kulcsos aláírás szükséges.

A Windows PC-n a `.github/workflows/microphone-windows.yml` parancsai követhetők a Visual Studio C++ buildeszközökkel. A csomagolás parancsa a repó gyökerében: `python build.py --portable --flutter --skip-portable-pack`. A Flutter-híd és a natív függőségek előkészítése itt is szükséges. A munkakönyvtárat és a vcpkg/Cargo gyorsítótárakat őrizd meg a következő fordításhoz.

Az NDK-fejlécjavítás teljes Android-fordítással még nincs igazolva; a GitHub próbát a felhasználó kérésére leállítottuk, a helyi build és a telefonos teszt következik.
