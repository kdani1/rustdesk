# Távoli mikrofon — fejlesztői változat

Ez a fork a vezérlő eszköz mikrofonját továbbítja a vezérelt számítógép virtuális hangbemenetére. A cél, hogy az ott futó alkalmazások az alapértelmezett mikrofonként használhassák.

**Állapot: kísérleti fejlesztés.** A forráskód megléte nem jelent kész telepítőt vagy minden platformon igazolt működést. Az ellenőrzések és a fennmaradó feltételek a [tesztleírásban](TESTING.md) szerepelnek. A hivatalos RustDesk 1.5.0 kiadás nem tartalmazza ezt a változtatást: a saját build kell mindkét végpontra. A meglévő hbbs/hbbr szervert nem kell lecserélni.

## Használat

1. A **vezérelt gépen** telepítsd az alább felsorolt virtuális hangeszközt, majd engedélyezd a RustDesk Beállítások → Általános → Mikrofon továbbítása → Továbbított mikrofon fogadásának engedélyezése opciót.
2. A **csatlakozó gépen** a REC mellett megjelenő mikrofon gombbal indítsd vagy állítsd le a továbbítást. Androidon az alsó eszköztáron, illetve a csevegés menüjében található.
3. Az automatikus indításhoz a csatlakozó kliens általános beállításaiban kapcsold be a Mikrofon automatikus továbbítása kapcsolót. Alapból mindkét engedély kikapcsolt.
4. A narancssárga ikon az indítást jelzi, a piros mikrofon a megnyitott virtuális hangutat. A célalkalmazásban is ellenőrizd a jelszintet: ez külön bizonyítja, hogy tényleges beszéd érkezik.
5. A célalkalmazás az alapértelmezett rendszerbemenetet használja, vagy válaszd ki benne a virtuális eszközt. Egy már futó hangfelvételt az alkalmazás viselkedésétől függően újra kell indítani.
6. Bontáskor a korábbi rendszerbemenet visszaáll, amennyiben közben nem választottál másik eszközt. A mikrofon gomb kikapcsolása is leállítja a továbbítást. Egyszerre egy fogadott mikrofonfolyam használható.

A mikrofonhoz helyi operációsrendszer-engedély szükséges. Hanghívás és mikrofontovábbítás ugyanabban a munkamenetben nem fut együtt. A fogadó hangengedélyének visszavonása leállítja a mikrofont is. Ne küldj ide jelszót, privát szerverkulcsot vagy személyes konfigurációt.

## Windows

**Küldés:** saját Windows build, Beállítások → Adatvédelem és biztonság → Mikrofon → asztali alkalmazások mikrofonhozzáférése bekapcsolva. A kívánt fizikai mikrofon legyen alapértelmezett.

**Fogadás:** telepítsd a [VB-CABLE](https://vb-audio.com/Cable/) gyártói illesztőprogramját, majd az [AudioDeviceCmdlets](https://github.com/frgnca/AudioDeviceCmdlets) modult Windows PowerShellben:

```powershell
Install-Module -Name AudioDeviceCmdlets -Scope CurrentUser
Get-AudioDevice -List
```

A RustDesket ugyanazon bejelentkezett felhasználó munkamenetében futtasd, ahol a modul és a célalkalmazás elérhető. A szolgáltatásfiókos, bejelentkezés előtti és többfelhasználós Windows-fogadás nincs igazolva. A kód a `CABLE Input` lejátszóeszközbe ír és a `CABLE Output` felvételi eszközt állítja alapértelmezetté; a normál és kommunikációs bemenet korábbi értékét külön menti és állítja vissza. A VB-CABLE külön gyártói szoftver, nem része ennek az open source repónak.

## macOS

**Küldés:** saját Mac build, mikrofonengedély a Rendszerbeállítások → Adatvédelem és biztonság → Mikrofon oldalon.

**Fogadás:** telepítsd a [BlackHole 2ch](https://github.com/ExistentialAudio/BlackHole) hivatalos csomagját. A telepítő rendszergazdai jelszót és újraindítást kérhet. Ha már van Homebrew: `brew install --cask blackhole-2ch`. Ellenőrizd az Audio MIDI Setupban, hogy a **BlackHole 2ch** bemenet és kimenet is megjelent.

A továbbítás csak a rendszer **bemenetét** váltja át BlackHole-ra. A hangszórók alapértelmezett kimenetét ne állítsd BlackHole-ra. A helyi forrásból épített app aláírása eltér a hivatalos kiadásétól; a macOS jogosultságokat újra kérheti. Működő stabil telepítést csak a tesztek után cserélj le, és tartsd meg a visszaállítható példányt.

**Lehajtott fedél:** a virtuális mikrofon nem tartja ébren a gépet. A szerver és a kliens csak ébren, működő hálózattal használható; a csukott fedeles üzemmódot külön kell ellenőrizni a hardverrel. Ez a fejlesztés nem tiltja le a fedélérzékelőt és nem garantálja a csukott fedeles működést.

## Linux

**Küldés:** saját Linux build, bejelentkezett grafikus munkamenet és elérhető alapértelmezett mikrofon.

**Fogadás:** PulseAudio, vagy PipeWire PulseAudio-kompatibilitási szolgáltatással és `pactl` paranccsal. Ellenőrzés:

```sh
pactl info
pactl get-default-source
```

A kód a felhasználó hangkiszolgálójában ideiglenes null sinket és remap source-ot hoz létre. A bemenet neve `rustdesk_microphone_input`; bontáskor visszaállítja a korábbi bemenetet és eltávolítja a létrehozott modulokat. Root/headless munkamenetben, működő felhasználói hangkiszolgáló nélkül nem használható. A PulseAudio és PipeWire kombinációk végponti tesztje még szükséges.

## Android

A saját APK szükséges. A hivatalos APK és az egyéni aláírás nem frissíthető egymásra: mentés után külön tesztkészüléken használd, vagy a régi alkalmazás eltávolítása után telepítsd. Engedélyezd a mikrofont, csatlakozz a célgéphez, majd használd az alsó sáv mikrofon gombját. Az automatikus kapcsoló az alkalmazás beállításaiban található. Tartsd előtérben teszt közben; az Android háttérbeli hangrögzítési korlátai érvényesek.

Android **küldőként** kapott megvalósítást; teljes rendszerszintű virtuális mikrofonként nem fogad. Androidon egy másik tetszőleges alkalmazás mikrofonbemenetének lecserélése nincs implementálva.

## iPhone/iPad és böngésző

Ebben a változatban nem támogatott a mikrofontovábbítás. A távoli képernyőkezelés támogatását ez nem módosítja. Nem kínálunk nem létező iOS virtuálismikrofon-telepítőt.

## Kapcsolódás internetről

Mindkét kliensben ugyanazt az ID-szervert és annak **nyilvános** kulcsát állítsd be. Külső kliensen elérhető publikus IP/DNS kell; a `192.168.x.x` cím csak a helyi hálózaton jó. Az ID-szerver TCP 21115/21116 és UDP 21116, a relay TCP 21117 portját a kiszolgálóra kell irányítani. A mikrofon a meglévő titkosított munkameneten halad; külön mikrofonport nem kell.

A konkrét saját szerver címe és a gép csatlakozási adatai a külön, privát **Rust-Remote** repóban vannak. A nyilvános forkba nem kerül személyes szerverkonfiguráció. Első próbánál a telefon Wi-Fi-jét kapcsold ki. A portok elérhetősége önmagában nem bizonyít sikeres RustDesk-azonosítást vagy mikrofonhangot.

## Fordítás

Az upstream [buildleírás](https://rustdesk.com/docs/en/dev/build/) és az adott platform [Flutter build workflow-ja](../../.github/workflows/flutter-build.yml) tartalmazza a verziókat és rendszerfüggőségeket. Ebből a forkból a `feature/remote-microphone` ágat klónozd rekurzív submodule-okkal. A `.github/workflows/bridge.yml` szerint generáld a Rust–Dart kötéseket. macOS-en a `script/build_and_run.sh --build-only` a helyi build belépési pontja, miután a toolchain, vcpkg, CocoaPods és Flutter rendelkezésre állnak.
