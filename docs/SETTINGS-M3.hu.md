# M3 – Beállítások kód nélkül (teendők a tulajdonosnak)

Ez a leírás a roadmap M3-as lépését írja le. Ezekhez nem kell új RustDesk-build: csak
Windows- és RustDesk-beállításokat kell módosítani. Rendszerbeállításokról van szó, ezért ezeket
neked kell elvégezned. Minden lépésnél leírom, hogyan lehet visszaállítani.

Fontos tudnivalók előtte:

- Rendszergazdai PowerShell kell hozzá (Start menü > „PowerShell” > jobb klikk > „Futtatás
  rendszergazdaként”). Ahol csak „PowerShell” szerepel, ott elég a normál ablak.
- A Wi-Fi-adapter speciális beállításainak módosításakor az adapter néhány másodpercre
  lekapcsol. **Ezt ne RustDesk-kapcsolaton keresztül csináld**, hanem a gép előtt ülve. Ha
  mégis távolról teszed, a kapcsolat megszakad, és lehet, hogy csak kézzel tudod helyreállítani.
- Lépésenként haladj, és minden lépés után mérj (lásd a „Mérés” részt). Így kiderül, melyik
  változtatás mit hozott.
- A mentéseket tedd egy mappába, például `C:\M3-mentes`. A parancsok ezt a helyet használják:

```powershell
New-Item -ItemType Directory -Force C:\M3-mentes
```

## 0. Alapmérés előtte

A Dellen (normál PowerShell) futtasd le a `dell-diag.ps1`-et egy futó RustDesk-munkamenet
közben. Az eredmény az asztalra kerül `dell-diag.txt` néven. Ezt nevezd át
`dell-diag-elotte.txt`-re.

Jegyezd fel ezt is: hány „Set fps” sor van a Dell nézőnaplójában egy 5 perces munkamenet alatt,
és mi a `LATENCY client` sorokban a `net+server(est)` p95-e. A napló helye:
`%APPDATA%\RustDesk\log`.

## 1. Dell: energiamód „Legjobb teljesítmény” hálózati tápon

1. Dugd be a Dell töltőjét.
2. Beállítások > Rendszer > Tápellátás és akkumulátor > Energiamód. Angolul:
   Settings > System > Power & battery > Power mode.
3. Válaszd ezt: **Legjobb teljesítmény** (Best performance).

A Windows ezt az értéket külön tárolja töltőn és akkumulátoron. Itt csak a töltős értéket
kell átállítani. Az akkumulátoros maradhat a régi.

Ellenőrzés: a `dell-diag.ps1` „power” részében látszik az aktív energiaséma. A sémák listáját
így kérheted le:

```powershell
powercfg /getactivescheme
powercfg /list
```

**Visszaállítás:** ugyanott válaszd vissza a korábbi módot (általában „Kiegyensúlyozott”).

## 2. Dell: a Wi-Fi energiatakarékossága legyen „Maximális teljesítmény”

Ez az energiaséma „Vezeték nélküli adapter beállításai > Energiatakarékos mód” értéke.
Hálózati tápon (AC) és akkumulátoron (DC) is állítsd át.

### 2a. Először ellenőrizd a GUID-okat

Rendszergazdai PowerShellben:

```powershell
powercfg /q SCHEME_CURRENT 19cbb8fa-5279-450e-9fac-8a3d5fedd0c1 > C:\M3-mentes\wifi-power-elotte.txt
Get-Content C:\M3-mentes\wifi-power-elotte.txt
```

Ezt kell látnod:

- egy „Alcsoport GUID-ja: 19cbb8fa-5279-450e-9fac-8a3d5fedd0c1 (Vezeték nélküli adapter
  beállításai)” sort (angolul: Wireless Adapter Settings);
- alatta egy „Energiagazdálkodási beállítás GUID-ja: 12bbebe6-58d6-4636-95bb-3217ef867c1a
  (Energiatakarékos mód)” sort (angolul: Power Saving Mode);
- a lehetséges értékeket: 0 = Maximális teljesítmény, 1 = Alacsony energiatakarékosság,
  2 = Közepes energiatakarékosság, 3 = Maximális energiatakarékosság;
- a jelenlegi AC és DC indexet („Váltakozó áramú…” és „Egyenáramú…” sor).

**Ha a GUID-ok nálad mások, ne a lenti parancsot futtasd, hanem a saját GUID-jaidat írd bele.**
Ha az alcsoport egyáltalán nem jelenik meg, akkor rejtett. Ezzel láthatóvá teheted:

```powershell
powercfg -attributes 19cbb8fa-5279-450e-9fac-8a3d5fedd0c1 12bbebe6-58d6-4636-95bb-3217ef867c1a -ATTRIB_HIDE
```

### 2b. Beállítás

```powershell
powercfg /setacvalueindex SCHEME_CURRENT 19cbb8fa-5279-450e-9fac-8a3d5fedd0c1 12bbebe6-58d6-4636-95bb-3217ef867c1a 0
powercfg /setdcvalueindex SCHEME_CURRENT 19cbb8fa-5279-450e-9fac-8a3d5fedd0c1 12bbebe6-58d6-4636-95bb-3217ef867c1a 0
powercfg /setactive SCHEME_CURRENT
```

Utána a 2a-ban látott `powercfg /q` parancs már 0-t mutat mindkét indexnél.

Az Eszközkezelőben is nézd meg: Eszközkezelő (`devmgmt.msc`) > Hálózati adapterek > a Wi-Fi
adapter > Tulajdonságok > Energiagazdálkodás fül. Itt vedd ki a pipát ebből: „A számítógép
kikapcsolhatja ezt az eszközt az energiatakarékosság érdekében”. Ha nincs ilyen fül, hagyd ki ezt
a részt.

**Visszaállítás:** ugyanezek a parancsok, de 0 helyett a `wifi-power-elotte.txt`-ben látott
régi AC, illetve DC index. Ha az Energiagazdálkodás fülön kivetted a pipát, tedd vissza.

## 3. Dell és Alza: roaming, MIMO-energiatakarékosság, sávválasztás

Ezek a Wi-Fi-driver speciális beállításai. A nevük és az értékük driverenként és nyelvenként
más. Ezért előbb listázd ki és mentsd el őket, és csak azután módosíts.

### 3a. Listázás és mentés (mindkét gépen)

Rendszergazdai PowerShellben. A Wi-Fi-adapter neve általában „Wi-Fi”, ezt az első parancs
mutatja meg:

```powershell
Get-NetAdapter | Format-Table Name, InterfaceDescription, Status
Get-NetAdapterAdvancedProperty -Name "Wi-Fi" | Format-Table DisplayName, DisplayValue, ValidDisplayValues -AutoSize -Wrap
Get-NetAdapterAdvancedProperty -Name "Wi-Fi" | Select-Object DisplayName, DisplayValue, RegistryKeyword, RegistryValue |
    Export-Csv C:\M3-mentes\wifi-advanced-elotte.csv -NoTypeInformation -Encoding UTF8
```

### 3b. Mit állíts át

A Dellen (valószínűleg Intel adapter) ezeket keresd. Ha a driver magyarul írja ki a nevüket,
a jelentésük alapján keresd meg őket:

| Beállítás (Intel, angolul) | Új érték | Mire jó |
|---|---|---|
| Roaming Aggressiveness | `1. Lowest` | ritkábban keres más hozzáférési pontot, ritkábban akad meg pár száz ms-ra |
| MIMO Power Save Mode | `No SMPS` | az adapter nem kapcsol le antennákat energiatakarékosságból |
| Preferred Band | `3. Prefer 6GHz band`, ha a router tud 6 GHz-et; különben `2. Prefer 5GHz band` | ne a zsúfolt 2,4 GHz-es sávra csatlakozzon |

Az Alzán (Realtek 8851BE) a nevek mások. A listában keress hasonló jelentésűeket: roaming
érzékenység vagy agresszivitás (a legalacsonyabbra), energiatakarékos mód (ki), előnyben
részesített sáv (5 GHz). Amit nem találsz, azt hagyd ki.

Beállítás PowerShellben. A `-DisplayName` és a `-DisplayValue` pontosan az legyen, amit a 3a
lista mutat:

```powershell
Set-NetAdapterAdvancedProperty -Name "Wi-Fi" -DisplayName "Roaming Aggressiveness" -DisplayValue "1. Lowest"
Set-NetAdapterAdvancedProperty -Name "Wi-Fi" -DisplayName "MIMO Power Save Mode" -DisplayValue "No SMPS"
Set-NetAdapterAdvancedProperty -Name "Wi-Fi" -DisplayName "Preferred Band" -DisplayValue "2. Prefer 5GHz band"
```

Grafikus felületen is beállíthatod: Eszközkezelő > Hálózati adapterek > Wi-Fi adapter >
Tulajdonságok > Speciális fül.

Ellenőrzés: újracsatlakozás után nézd meg a sávot és a csatornát. A „Rádió típusa”, a „Sáv” és
a „Csatorna” sort figyeld:

```powershell
netsh wlan show interfaces
```

**Visszaállítás:** a `wifi-advanced-elotte.csv` régi értékeivel ugyanígy, vagy egyben (ez
minden speciális beállítást a driver alapértékére állít, nem a korábbi értékekre):

```powershell
Reset-NetAdapterAdvancedProperty -Name "Wi-Fi" -DisplayName "*"
```

## 4. Dell: QoS-jelölés a rustdesk.exe forgalmára (DSCP 40 és 802.1p 5)

A Dell kimenő RustDesk-forgalma (bemenet, ACK-ok) DSCP 40 jelölést kap, és 802.1p 5-ös
prioritást. A 802.1p érték is kell, mert a Wi-Fi-driver ebből választja ki a WMM-sort
(videósor). Ha csak DSCP-t állítasz be, és nincs hatása, az még nem bizonyít semmit.

Rendszergazdai PowerShellben:

```powershell
New-NetQosPolicy -Name "RustDesk-M3" -AppPathNameMatchCondition "rustdesk.exe" -DSCPAction 40 -PriorityValue8021Action 5
Get-NetQosPolicy -Name "RustDesk-M3"
```

Utána indítsd újra a RustDesk-munkamenetet.

### 4a. Ellenőrzés pktmonnal

Ellenőrizd, hogy a csomagok tényleg megkapják-e a jelölést. A DSCP 40 az IP-fejlécben a TOS
0xA0 értékként látszik. Rendszergazdai PowerShellben a Dellen, az Alza IP-címével:

```powershell
pktmon filter remove
pktmon filter add RustDesk -i <Alza-IP-címe>
pktmon start --capture --comp nics --pkt-size 128 -f C:\M3-mentes\qos.etl
# most 15-20 másodpercig használd a RustDesk-munkamenetet (egérmozgatás, gépelés)
pktmon stop
pktmon etl2txt C:\M3-mentes\qos.etl -o C:\M3-mentes\qos.txt
(Select-String -Path C:\M3-mentes\qos.txt -Pattern "tos 0xa0").Count
(Select-String -Path C:\M3-mentes\qos.txt -Pattern "tos 0x0,").Count
pktmon filter remove
```

- Ha az első szám nagyobb nullánál, a jelölés működik.
- Ha nulla, és a második szám nagy, akkor a policy nem hat.
- Ha az `etl2txt` nem létezik az adott Windowsban, használd ezt: `pktmon format
  C:\M3-mentes\qos.etl -o C:\M3-mentes\qos.txt`.
- Ha a szöveges kimenetben nincs „tos” mező, alakítsd pcapng-re (`pktmon etl2pcap
  C:\M3-mentes\qos.etl -o C:\M3-mentes\qos.pcapng`). Nyisd meg Wiresharkban, és szűrj erre:
  `ip.dsfield.dscp == 40`.
- IPv6-kapcsolatnál a mező neve „class”: keress a `class 0xa0` mintára.

A pktmon csak a DSCP jelölést látja. Azt nem, hogy a driver melyik WMM-sorba tette a csomagot.

Ha nincs jelölés, akkor egy domainbe nem léptetett gépen gyakran ez a beállítás kell hozzá.
Utána indítsd újra a gépet, és ismételd meg a 4a-t:

```powershell
New-Item -Path "HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\QoS" -Force | Out-Null
New-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\QoS" -Name "Do not use NLA" -PropertyType String -Value "1" -Force
```

LAN-on, ha mindkét gépnek van globális IPv6-címe, a közvetlen út IPv6 KCP-re válthat (lásd az
5. lépést). Ilyenkor a RustDesk saját LAN-QoS-a ezt az utat WAN-nak veszi, így ott nem jelöl.
A fenti Windows-policy viszont a rustdesk.exe minden forgalmát jelöli.

**Visszaállítás:**

```powershell
Remove-NetQosPolicy -Name "RustDesk-M3" -Confirm:$false
# csak ha a registry-értéket is beállítottad:
Remove-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\QoS" -Name "Do not use NLA"
```

## 5. Dell: UDP-lyukasztás és IPv6 P2P bekapcsolása a RustDeskben

Ezek nézőoldali beállítások, ezért csak a Dellen kell bekapcsolni őket.

**Előtte** nézd meg, milyen rendezvous-szervert (hbbs) használsz. Ezt a RustDesk Beállítások
> Hálózat > ID/relay szerver részénél látod. Ha saját szervered van, nézd meg a hbbs verzióját
(például a docker image címkéjét, vagy a hbbs naplójának első sorait). A RustDesk szerver
kiadási jegyzeteiből derül ki, hogy támogatja-e az UDP-s és az IPv6-os lyukasztást. Ha a
szerver nem támogatja, a bekapcsolás nem árt, de nem is hoz semmit.

Bekapcsolás:

1. RustDesk > Beállítások > Általános > Egyéb.
2. Pipáld be: **UDP résszűrés engedélyezése** (angolul: Enable UDP hole punching).
3. Pipáld be: **IPv6 P2P kapcsolat engedélyezése** (Enable IPv6 P2P connection).
4. A **TCP résszűrés engedélyezése** maradjon bekapcsolva.
5. A WebRTC-t még ne kapcsold be. Az az M8 része, külön mérjük.
6. Bontsd a munkamenetet, és csatlakozz újra.

Tudnivaló: az IPv6-teszthez a RustDesk nyilvános STUN-szervereket kérdez meg (Cloudflare,
Google). Ezen az `ice-servers` beállítás nem változtat.

Ellenőrzés a Dell nézőnaplójában (`%APPDATA%\RustDesk\log`):

```powershell
Get-ChildItem "$env:APPDATA\RustDesk\log" -Recurse -Filter *.log | Sort-Object LastWriteTime -Descending |
    Select-Object -First 1 | Select-String -Pattern "TRANSPORT viewer|RACE " | Select-Object -Last 10
```

- A `TRANSPORT viewer` sorban a `type=` mutatja, mi lett az út: `UDP` és `IPv6` az új utak, `TCP` a
  régi közvetlen út, `Relay` a kerülőút.
- A `RACE` sorok mutatják, melyik próbálkozás sikerült, és mennyi idő alatt.
- A `LATENCY client` sorok végén lévő `via=` mező ugyanezt mutatja 5 másodpercenként.

**Visszaállítás:** vedd ki a két pipát, és csatlakozz újra.

## Mérés (kapu)

Minden lépés után:

1. A Dellen futtasd a `dell-diag.ps1`-et munkamenet közben, és hasonlítsd össze a
   `dell-diag-elotte.txt`-vel. Nézd az energiasémát, a CPU-t és a dekóder-sorokat.
2. Számold meg a „Set fps” sorokat egy ugyanolyan hosszú, hasonló munkamenetben. Kevesebb a jobb.
3. Az 5. lépés után mérj LAN-on kívülről is: egyszer telefonos hotspotról, egyszer egy másik
   otthoni hálózatról. Mindkét helyzetben jegyezd fel:
   - a `via=` eloszlását (melyik út hányszor jött létre);
   - a `net+server(est)` p95-ét útvonalanként, a `LATENCY client` sorokból.

Mire számíthatsz (ezek becslések, nem mért értékek):

- Az energiamódtól kevesebb lassítás (throttling) a Dellen.
- A DSCP-től néhány ms javulás az ACK és a bemenet p95-ében. De az is lehet, hogy semmi.
- Az UDP-től és az IPv6-tól interneten elmarad a relay-kerülő, ahol van közvetlen út. Ez 5–40 ms
  RTT-t jelenthet. A KCP egy RTT alatt pótolja az elveszett csomagot, egy TCP-újraküldés viszont
  száz ms-okig is tarthat.

## Minden visszaállítása egyben

1. Energiamód: vissza a régire (1. lépés).
2. Wi-Fi-energiatakarékosság: `powercfg /setacvalueindex` és `/setdcvalueindex` a régi
   indexekkel, utána `powercfg /setactive SCHEME_CURRENT` (2. lépés).
3. Wi-Fi speciális beállítások: a mentett CSV értékei, vagy `Reset-NetAdapterAdvancedProperty`
   (3. lépés). Ezt is a gép előtt ülve csináld.
4. `Remove-NetQosPolicy -Name "RustDesk-M3"`, és ha beállítottad, a „Do not use NLA” értéket is
   töröld (4. lépés).
5. A két RustDesk-pipa ki (5. lépés).
