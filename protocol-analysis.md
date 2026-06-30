# NEEWER Studio 5.7.0 — Remaining Protocol Analysis

Reverse-engineered from the decompiled Android app `neewer.nginx.annularlight` (jadx output at
`reference/decompiled/sources/`). Central command builder: `defpackage/cn.java`. Bytes in the
decompiled source are signed Java `byte` literals; this document gives the unsigned hex
(e.g. `-127` = `0x81`, `120` = `0x78`).

---

## What's newly mapped vs still unknown

### Newly mapped (this pass)
- **Complete opcode table** — every `0x78`-framed command/response in the app (≈70 opcodes), see below.
- **Pixel / Pixel-stick protocol** — it is **effect-descriptor + palette**, NOT a per-LED framebuffer.
  Opcodes `0xB0/0xB1/0xB2` (Pixel, 10 effects) and `0xD5/0xD6/0xD7` (Pixel12, 12 effects). Each
  effect is split into 2–3 sub-frames `[effectId, subIndex, …]`; colours are 3-byte triplets via
  `createColoByteArray` (CCT `00 cct gm`, HSI `(1x) hueLo sat`, black `20 00 00`).
- **"FX Flow" = the "Streamer" subsystem** (the string "Flow" is not used in code). Realtime group
  play `0xC0`; full multi-fixture download `0xBF` where each fixture gets a 10-byte record
  `[position, blockCount, startOffsetBE16, MAC6]` — position is the device's index in the group list.
- **DMX channel/address assignment** — `0xCA` with sub-commands (1=enable, 2=set group+DMX address,
  4=play); DMX effect download uses `0xCB` (frame header) + `0xCC` (per-block colour).
- **Firmware version read** — request `0x80` (direct) / `0x9E` (by-MAC); replies `0x00` / `0x08` carry
  the 3 version bytes at offsets 5–7 / 11–13.
- **OTA/DFU** — two transports: stock **Nordic Secure DFU** (`.zip`, service `0000fe59…`) for
  "DFU"-mode devices, and a **custom `0x78` block protocol** (`0xD0` probe → `0x96` header →
  `0x97`/`0xCF` image blocks, ACK `0x06`) for "OTA"-mode devices, over the normal control service.
- **Notify/response parser** fully decoded — every reply tag (`0x00,0x02–0x09,0x11–0x1B,0x1D,0x7F`).
- **GATT UUIDs** — control service `69400001-…`, write `…0002`, notify `…0003`. OTA rides the same
  service (there is **no** `7f51…` characteristic anywhere in the app).
- **Auth** — there is **none** at the BLE layer. "Infinity Authenticated" is a UI label meaning the
  device was claimed under your account's server-assigned 32-bit `networkId`. `sendIdentityCheck`
  just transmits that plaintext `networkId`; no challenge/key/signature gates any command.
- **Group-id assignment** — the 32-bit `networkId` (advertised as `NW…&XXXXXXXX`, little-endian) is
  written to devices via `0x8C` (channel+netid), `0x9F` (add light), `0x9B` (sync), `0xA3` (identity).
- **Model→capability** — split across `nj0` (config table by integer light-type), `ck0`/`qj0`/`vy4`
  predicate lists, and a runtime 2.4G flag from the BLE name.

### Still unknown / not determinable from source
- **Exact effect-mode → visual-pattern mapping** for Streamer/flow (`effectEnum`/`mStreamerMode` ints
  are data-driven from server JSON, not enumerated as constants).
- **Per-byte units/ranges** for several effect params (`sectionType`, `runStyle`, `movement`,
  `direction` encodings) — passed through verbatim from UI state, no in-code annotation.
- **`BleDevice.getLightType(name)`** body (the advertised-name → integer-type decoder) is too large
  for jadx to decompile ("instruction units count: 3291"); the type-int→name table in `ck0` is the
  recoverable half.
- **WiFi provisioning payload** (`setWifiConnectValue`, opcode family `0x03` reply) only partially read.
- No CRC/signature anywhere — integrity is the single trailing additive checksum byte only.

---

## Frame format (recap + confirmation)

```
[0]=0x78  [1]=opcode  [2]=len  [3 .. 3+len-1]=payload  [last]=checksum
```
- `len` counts the payload bytes only (excludes header `78 op len` and the trailing checksum).
- **Checksum** = `Σ(all bytes except last) & 0xFF`, written into the last byte
  (`cn.checkSum`, `cn.java:1303`). Negative bytes are taken `+256` first.
- **OTA exception:** for devices where `qj0.getDeviceClassify(type)==6`, the header byte is `0x85`
  instead of `0x78` (`cn.java:1294, 2761, 1290`).
- **MAC-addressed (Infinity/2.4G single)** frames carry the 6-byte target MAC at bytes `[3..8]`; the
  app sends them to the group **master** (`User.getMasterDevice`), which relays over 2.4G.
- **Group/channel** frames carry `networkId` as 4 little-endian bytes at `[3..6]` and a channel byte
  at `[7]`. Some "New" variants carry only the low 2 bytes of `networkId`.
- **`networkId`** = the account/group id, server-assigned, advertised in the device name as
  `NW<type>&<8 hex, little-endian>`; `&FFFFFFFF` = unassigned (`BleDevice.java:133`).

---

## Complete opcode table

Header `0x78` and trailing checksum omitted from the "payload" column. `MAC6` = 6 target-MAC bytes;
`NET4` = networkId little-endian 32-bit; `NET2` = networkId low 16-bit; `CH` = group channel byte.

### Direct BLE control (no addressing)

| Hex | Dec | Name / builder | Payload layout | Source |
|----|----|----|----|----|
| `0x81` | 129 | Power on/off | `<01 on \| 02 off>` | `cn.java:3151` (`powerGroupOff` literal `78 81 01 02`) |
| `0x82` | 130 | Brightness only (`setLightValue`) | `<bri>` | `cn.java:986`, `AdjustLightFragment:81` |
| `0x83` | 131 | CCT only (`setLightValue`) | `<cct>` | `cn.java:950`, `AdjustLightFragment:95` |
| `0x84` | 132 | Enter/req adjust mode (`setLightValue(132,0,0)`) | *(none)* | `AdjustLightViewModel:415` |
| `0x85` | 133 | **Light state query** (`getLightPowerState`/`queryLightState`) | *(none)* → `78 85 00 FD` | `cn.java:2899,3277`; `setLightValue(133,…)` `MultiDeleteViewModel:143` |
| `0x86` | 134 | **HSI** (`setRGBLightValue(134,…)`) | `<hueLo hueHi sat bri 00>` | `HSIControlFragment:1458`, `cn.java:1013` |
| `0x87` | 135 | **CCT full** (`setRGBLightValue(135,…)`) | `<INT CCT GM curveType unit>` (2–5 bytes) | `CCTControlFragment:1016`, `cn.java:996` |
| `0x88` | 136 | **Scene / old FX** (`setRGBLightValue(136,…)`) | `<INT (effType*3+mode)>` | `AiCustomUtils:1979`, `cn.java:1022` |
| `0x8A` | 138 | Device config request (`setLightValue(138,0,0)`) | *(none)* | `GLCctViewModel:55`, `WifiLightViewModel:99` |
| `0xA8` | 168 | RGBCW direct (`createRGBCWCommand`) | `<R G B C W ?? 00>` | `cn.java:2728` |
| `0xB9` | 185 | Colour-coordinate (CIE xy) direct | `<idx xLo xHi yLo yHi i3>` | `cn.java:1365` |
| `0xAF` | 175 | Colour-paper (gel) direct | `<paperLo paperHi i3 i4 i6 brand i5>` (brand: ROSCO=1, LEE=2) | `cn.java:1466` |
| `0xB2` | 178 | **Pixel effect** (BLE) | `<len> <effectData>` | `cn.java:2461` |
| `0xD7` | 215 | **Pixel12 effect** (BLE) | `<len> <effectData>` | `cn.java:2180` |
| `0x61` | 97 | LP40s light set (`setLp40sLight`) | `<8 bytes>` | `cn.java:3836` |
| `0x62` | 98 | LP40s config query (`getLp40sConfig`) | `00` | `cn.java:2903` |
| `0xF0` | 240 | Factory test / fan switch | `AA` (`setLightValue(240,1,170)` / `sendFanSwitch 78 F0 01 AA`) | `cn.java:3391`, `RgbTestViewModel:215` |
| `0xF5` | 245 | Factory test | `CC` (`setLightValue(245,1,204)`) | `SettingActivity:410` |

### Infinity / 2.4G — MAC-addressed (relayed by master)

| Hex | Dec | Name / builder | Payload layout | Source |
|----|----|----|----|----|
| `0x8D` | 141 | **Power by MAC** (`setContinuityRGB1EffectValue(141,1,129,…)`) | `MAC6 81 <01\|02>` | `cn.java:3992` |
| `0x8E` | 142 | **State read / power status** (inner `0x85`) | `MAC6 85` → reply `0x04`; also `getInfinityPowerStatus` | `cn.java:862,2877` |
| `0x8F` | 143 | DIY effect by MAC (`createDIYEffect24GCommand`) | `MAC6 86 <effLo effHi p1 p2 00>` | `cn.java:1515,3884` |
| `0x90` | 144 | CCT/HSI effect by MAC (`setRGB1EffectValue(144,…)`) | `MAC6 <sub> <data>` | `cn.java:3884` callers |
| `0x95` | 149 | **Battery query by MAC** (`getBatteryByMac`) | `MAC6` → reply `0x05` | `cn.java:2794` |
| `0x99` | 153 | Find-light (identify) (`sendFindLight`) | `MAC6` | `cn.java:3399` |
| `0x9A` | 154 | Control slave light by netid (`controlSlaveDeviceLight`) | `NET4 <01\|02>` | `cn.java:1345` |
| `0x9D` | 157 | Online-status query (`getInfinityOnlineStatus`) | `MAC6 NET4` → reply `0x7F` | `cn.java:2869` |
| `0x9E` | 158 | **Version read by MAC** (`getDeviceInfoByMac`) | `MAC6` → reply `0x08` | `cn.java:2847` |
| `0xA4` | 164 | Dimming-curve set (`sendCurveType`) | `MAC6 87 <curve0 curve1 curve2> <i2>` | `cn.java:3333` |
| `0xAB` | 171 | Booster data by MAC (`getBoosterDataByMac`) | `MAC6 <01\|02>` | `cn.java:2814` |
| `0xAC` | 172 | Booster state by MAC (`getBoosterStateDataByMac`) | `MAC6` | `cn.java:2830` |
| `0xA9` | 169 | RGBCW by MAC (`createRGBCWCommandWithMac`) | `MAC6 A8 <R G B C W i8>` | `cn.java:2736` |
| `0xAD` | 173 | Colour-paper by MAC | `MAC6 paperLo paperHi i3 i4 i6 brand i5` | `cn.java:1444` |
| `0xB0` | 176 | **Pixel effect** by MAC | `MAC6 <effectData>` | `cn.java:2449` |
| `0xB3` | 179 | Temp + fan-mode query (`getDeviceTemperatureAndFanMode`) | `MAC6` → reply `0x12` | `cn.java:2858` |
| `0xB4` | 180 | Fan-mode set (`sendFanModeCommand`) | `MAC6 <mode>` | `cn.java:3374` |
| `0xB7` | 183 | Colour-coordinate by MAC | `MAC6 idx xLo xHi yLo yHi i3` | `cn.java:1389` |
| `0xBD` | 189 | Double-light state query (`queryDoubleLightState`) | `MAC6` → reply `0x13` | `cn.java:3261` |
| `0xBE` | 190 | **Double-light state set** (`setDoubleLightState`) | `MAC6 <i2>` | `cn.java:3791` |
| `0xC4` | 196 | Streamer-support query (`getIsSupportStreamer`) | `MAC6` → reply `0x14` | `cn.java:2889` |
| `0xD3` | 211 | **DIY-gradient toggle / music-gradient (single)** | `MAC6 <flag>` | `setDIYGradient cn.java:3780`; `setMusicGradient cn.java:3845` |
| `0xD5` | 213 | **Pixel12 effect** by MAC | `MAC6 <effectData>` | `cn.java:2168` |
| `0xD9` | 217 | RGBW by MAC (`createRGBWCommandWithMac`) | `MAC6 <bri colorIdx>` | `cn.java:2743` |
| `0xA6` | 166 | Lighting-effect status (single) (`createLightingEffectStatusCommand(str,i)`) | `MAC6 <i2>` | `cn.java:2157` |
| `0xD1` | 209 | Scheduled-tasks query (`getScheduledTasks`) | `MAC6` → reply `0x1B` | `cn.java:2932` |
| `0xD2` | 210 | **Scheduled-tasks set** (`setScheduledTasks`) | `MAC6 <en> <weekdays> <cdEn> <ts4> <countdown4> <i3>` | `cn.java:3938` |
| `0xD8` | 216 | Factory test (`sendFactoryTest`) | `MAC6 <mode> <i3 i4 i5 i6>` | `cn.java:3355` |

### Infinity / 2.4G — group/channel addressed

| Hex | Dec | Name / builder | Payload layout | Source |
|----|----|----|----|----|
| `0x92` | 146 | **DIY by channel** (`createDIYEffect24GByChCommand` → `setRGB1GroupLightValue(146,…)`) | `NET4 CH 86 <effLo effHi p1 p2 00>` | `cn.java:1511,3898` |
| `0x93` | 147 | HSI by channel (`setRGB1GroupLightValue(147,…)`) | `NET4 CH 87 <data>` | `MusicFXControlViewModel:603` |
| `0x94` | 148 | Old-FX by channel (`setRGB1GroupLightValue(148,…)`) | `NET4 CH <EFFECT_MODE_OLD> <data>` | `SceneControlFragment:1718` |
| `0x98` | 152 | **Power by channel** (group) | `NET4 CH 81 <01\|02>` (or `NET2 CH 81 …`) | `cn.java:3153,3207` |
| `0xA7` | 167 | Lighting-effect status by channel | `NET4 CH <status>` | `cn.java:4092` (`createLightingEffectStatusCommand(int,int)`) |
| `0xAA` | 170 | RGBCW by channel (`createRGBCWCommandWithChannel`) | `NET4 CH A8 <R G B C W i9>` | `cn.java:2732` |
| `0xB1` | 177 | **Pixel effect** by channel | `NET4 CH <effectData>` | `cn.java:2712` |
| `0xB8` | 184 | Colour-coordinate by channel | `NET4 CH idx xLo xHi yLo yHi i4` | `cn.java:1415` |
| `0xAE` | 174 | Colour-paper by channel | `NET4 CH paperLo paperHi i4 i5 i7 brand i6` | `cn.java:1486` |
| `0xC0` | 192 | **Streamer realtime group play** (`setStreamerGroupPlayValue`) | `NET4 CH <eff bri [bgBri] spd dir openState>` | `cn.java:3966` |
| `0xD4` | 212 | Music-gradient by channel (`setMusicInfinityGradient`) | `NET4 CH <flag>` | `cn.java:3856` |
| `0xD6` | 214 | **Pixel12 effect** by channel | `NET4 CH <effectData>` | `cn.java:2433` |

### Group/network management & registration

| Hex | Dec | Name / builder | Payload layout | Source |
|----|----|----|----|----|
| `0x8C` | 140 | **Assign channel + networkId** (`setRGB1PassValue` / `setNewRGB1PassValue`) | `MAC6 CH NET4` (New: `MAC6 CH NET2`) → reply `0x7F`/b9=`0x8C` | `cn.java:3915,3877` |
| `0x9B` | 155 | **Sync/register light** (`getSyncRGB1LightData`) | `MAC6 01 <idxLo idxHi>` → reply `0x7F`/b9=`0x9B` | `cn.java:870` |
| `0x9F` | 159 | **Add light + assign CH** (`sendRGB1CHDataAndLightAdd`) | `MAC6 01 CH NET4` → reply `0x7F`/b9=`0x9F` | `cn.java:3739,3707` |
| `0xA3` | 163 | **Identity check** (broadcast networkId) (`sendIdentityCheck`/`New`) | `NET4 <idx>` (New: `NET2 <idx>`) | `cn.java:3412,3677` |

### Battery / power-station / booster (CB200B, power supply)

| Hex | Dec | Name / builder | Payload layout | Source |
|----|----|----|----|----|
| `0xCD` | 205 | Battery info query (`getBatteryAllInfo/BaseInfo/PowerSupplyInfo`) | `00`=all, `01`=base, `09`=power-supply → reply `0x18` | `cn.java:2786,2790,2806` |
| `0xCE` | 206 | Battery strategy / limit / port | `cutOffBatteryAll 78 CE 02 00 00`; `setLimitPower <i2 i3>`; `setBatteryStrategy 05 <i2 …>`; `setSinglePortConnectState <z i2>` → reply `0x19` | `cn.java:2782,3832,3749,3962` |

### Firmware / OTA

| Hex | Dec | Name / builder | Payload layout | Source |
|----|----|----|----|----|
| `0x80` | 128 | **Version read (direct)** (`getDeviceInfo`) | *(none)* → `78 80 00 F8`, reply `0x00` | `cn.java:2841` |
| `0xD0` | 208 | OTA-type probe (`checkOtaType`) | *(none)* → reply `0x1A` | `cn.java:1290` |
| `0x96` | 150 | OTA update-info header (`createUpdateInfo`) | `<v1 v2 v3> <size BE32> <checkCode BE32> <nameASCII>` | `cn.java:2755` |
| `0x97` | 151 | OTA image block, 128-byte (`createFirmwareData`) | `<len8> <≤128 data>` | `cn.java:2125` |
| `0xCF` | 207 | OTA image block, 4096-byte (`createFirmwareData4096`) | `<len LE16 = data+1> <≤4096 data>` | `cn.java:2138` |

### Responses / notifications (`onCharacteristicChanged`, `cn.java:128`)

`b1` = `bArr[1]` (reply opcode).

| Hex | Dec | Meaning | Key fields | Source |
|----|----|----|----|----|
| `0x00` | 0 | Version reply (direct) | ver = bytes[5,6,7] | `cn.java:3011,3110` |
| `0x02` | 2 | Power status (direct) | bytes[3]: 1=on | `cn.java:161` |
| `0x03` | 3 | WiFi state | len 5 | `cn.java:3095` |
| `0x04` | 4 | Device power status (Infinity/MAC) / state-read reply | MAC bytes[3..8], mode/on at [9..10] | `cn.java:210,3007` |
| `0x05` | 5 | Battery reply (by MAC) | MAC[3..8], pct=`bArr[9]&0xFF` | `cn.java:257,3003,3099` |
| `0x06` | 6 | **OTA block ACK** | op=bytes[3]: 0=next,1=retry,2=restart,3=done,4=fail | `cn.java:3138`, `rb1.java:884` |
| `0x08` | 8 | Version reply (by MAC) | ver = bytes[11,12,13] | `cn.java:3015,3124` |
| `0x09`/`0x0A` | 9/10 | Light-effect pickup (music/audio reactive sensor) | `len ≥ bArr[2]+3` | `cn.java:3031` |
| `0x11` | 17 | CB200B booster | len 11 | `cn.java:2995` |
| `0x12` | 18 | Temperature / fan-mode | temp=`(bArr[9]&0xFF)-50` | `cn.java:2956,3019,3091` |
| `0x13` | 19 | Double-light state | len 11 | `cn.java:3023` |
| `0x14` | 20 | Streamer download message | MAC in payload | `cn.java:231,3087` |
| `0x15` | 21 | DMX switch state | bytes[4]: 1=on | `cn.java:244` |
| `0x17` | 23 | Streamer device message | hex-encoded | `cn.java:240` |
| `0x18` | 24 | Battery info | base if bytes[3]==1; power-supply if bytes[3]==9 | `cn.java:2967,2971,2983` |
| `0x19` | 25 | Battery port-connect / save-strategy | bytes[3] | `cn.java:2975,2987` |
| `0x1A` | 26 | OTA-type reply | len 5; bytes[3]==1 ⇒ "OTA_PRO" (4096B) else "OTA" (128B) | `cn.java:3041,3079` |
| `0x1B` | 27 | Scheduled-tasks reply | len ≥10 | `cn.java:3083` |
| `0x1D` | 29 | Factory reply | — | `cn.java:3027` |
| `0x7F` | 127 | **Multi-purpose Infinity reply** (online-status / channel-assign / register) | 12B(`[2]==8`) or 17B(`[2]==13`); sub-op echoed at `bArr[9]`: `0x8C`/`0x9B`(155)/`0x9F`(159) | `cn.java:160,274,338,370` |

---

## Topic 1 — Pixel / Pixel-stick protocol

**Key finding: there is no per-LED framebuffer command.** "Pixel" addressing in this app = palette
colour index, not physical LED index. Each effect = an effect-ID + scalar params + ≤8 colour
triplets; the firmware renders the named effect across its LEDs.

### Frame wrappers
- BLE: `78 B2 <len> <effectData> ck` (Pixel) / `78 D7 …` (Pixel12) — `cn.java:2461 / 2180`
- 2.4G MAC: `78 B0 <len> MAC6 <effectData> ck` / `78 D5 …` — `cn.java:2449 / 2168`
- Group: `78 B1 <len> NET4 CH <effectData> ck` / `78 D6 …` — `cn.java:2712 / 2433`

### Inner `effectData` (`createPixelEffectData`, `cn.java:2471`)
Returns a `List<byte[]>` of 2–3 sub-frames, each `[effectId, subIndex, payload…]`:
- sub-frame 0 = scalar params (brightness, speed, direction, colourNumber, runningStatus, …)
- sub-frame 1 = palette colours 0..5
- sub-frame 2 = palette colours 6..7 (appended only if `arrayList3.size() >= 3`, `cn.java:2701`)

Each sub-frame is individually wrapped and queued; frames are written one GATT write each, 80 ms
apart (`writeContinuityData(dev,true)` → `dv4.continuityWrite(…, 80L, …)`, `cn.java:4051`).
`splitArray` (20-byte MTU chunking) is **not** used for pixel sends — only for OTA.

### Colour triplet (`createColoByteArray`, `cn.java:658`)
- CCT: `[0x00, cct/100, gm]`
- HSI: `[(hue>>8)|0x10, hue&0xFF, saturation]`
- Black/off: `[0x20, 0x00, 0x00]`

### Pixel (10) vs Pixel12 (12)
`PixelEffectType` has 10 effects; its top 3 are **remapped on the wire** to IDs 10/11/12
(`cn.java:2634,2657,2680`), so base Pixel uses wire IDs `1–7,10,11,12`. `Pixel12EffectType` has 12
contiguous effects (IDs 1–12, adds `ColorStacking`/`PixelInterval`). The distinction is the
**opcode family** (`0xB0–0xB2` vs `0xD5–0xD7`) and effect-ID range — *not* a pixel count in the frame.
The "0x09-tag / 10-element array" hypothesis from the brief does **not** exist in the code.

Per-effect sub-frame-0 layouts are tabulated in the agent notes; representative:
`ColorReplacement(1)`: `[1,0, bri, colorNum, spd, dir, running]` (`cn.java:2488`).

---

## Topic 2 — "FX Flow" (Streamer)

The feature is named **Streamer** in code (`StreamerEffectsViewModel`); only TL-series sticks support
it (`vy4.isSupportStreamerEffect: type==59||type==115`, `vy4.java:32`).

### Realtime group play — `0xC0` (`cn.java:3966`)
```
78 C0 0A  NET4  CH  EE BR SP DR OS  ck      (5-value effect; EE=4 inserts bgBri → len 0x0B)
```
`EE`=effectEnum+1, `BR`=brightness, `SP`=speed, `DR`=direction, `OS`=openState
(`StreamerEffectsViewModel:1016`). Sent only to the master.

### Full multi-fixture download — `0xBF` (`dt3.java:213,243`; assembled `StreamerEffectsViewModel:1061`)
```
78 BF  LL HH                       header (len little-endian)
EM                                 effect mode = (effectEnum+1)&0xFF
[BG3]                              3-byte background packet, only if effectEnum==3
DN MCh MCl TCh TCl                 StepOne: deviceCount, modColorNum(BE16), totalColorNum(BE16)
P0 O0 S0h S0l  MAC0(6)             StepTwo per fixture: position, blockCount, startOffset(BE16), MAC6
P1 O1 S1h S1l  MAC1(6)             …one 10-byte record per fixture…
C0(3) C1(3) …                      colour-block packets (3 bytes each, same encoding as createColoByteArray)
KK                                 checksum = Σ(all) & 0xFF  (generateCheckSum2)
```
**Per-fixture position = the device's index in `deviceList`** (`StreamerEffectsViewModel:1083`); each
fixture is allocated a contiguous run of colour blocks `[startOffset, startOffset+blockCount)`. Block
count per fixture: TL60→8, TL120→16, type-1000→32 (`calculateColor`, `StreamerEffectsViewModel:219`).
Colour-block flag byte: `0x00`=CCT, `0x10`=HSI, `0x20`=off.

---

## Topic 3 — Firmware version read

- Direct: `78 80 00 F8` (`getDeviceInfo`, `cn.java:2841`). Reply opcode `0x00`, version =
  `bytes[5],bytes[6],bytes[7]` (`parseDeviceSoftwareVersion`, `cn.java:3110`). e.g. `1.1.9` arrives as
  `… 01 01 09 …`.
- By MAC: `78 9E 06 MAC6 ck` (`getDeviceInfoByMac`, `cn.java:2847`). Reply opcode `0x08`, version =
  `bytes[11],bytes[12],bytes[13]` (`parseDeviceSoftwareVersionWithMac`, `cn.java:3124`).
- Container: `entity/FirmwareVersion.java` (3 ints, default −1).

---

## Topic 4 — OTA / DFU

Two transports, chosen per device type by `qj0.getFirmwareUpdateMode(type)` (`rb1.java:300`,
`FirmwareNetControl.getUpdateType`: 1=DFU, 2=OTA).

### GATT (both normal control and custom OTA)
- Service `69400001-B5A3-F393-E0A9-E50E24DCCA99`, write `…0002`, notify `…0003` (`cn.java:4086,4124`).
- **No `7f51…` characteristic exists** in the app.

### Nordic Secure DFU (mode 1, `.zip`)
`kk0.startUpdate` (`kk0.java:81`): `DfuServiceInitiator(mac).setZip(path).start(ctx, DfuService.class)`,
`setUnsafeExperimentalButtonlessServiceInSecureDfuEnabled(true)`. Standard Nordic UUIDs:
Secure DFU service `0000fe59-0000-1000-8000-00805f9b34fb` (control `8ec90001…`, packet `8ec90002…`);
Legacy DFU service `00001530-1212-efde-1523-785feabcd123`.

### Custom `0x78` OTA (mode 2) — `rb1.java`
1. **Probe** `0xD0` (`78 D0 00 48`, `checkOtaType cn.java:1290`).
2. **Reply** `0x1A` (`isOtaProCommand cn.java:3079`): `bArr[3]==1` ⇒ "OTA_PRO" → 4096-byte blocks;
   else "OTA" → 128-byte blocks (`rb1.java:919`).
3. **Header** `0x96` (`createUpdateInfo cn.java:2755`): `<v1 v2 v3> <size BE32> <checkCode BE32> <name>`.
   `checkCode` = additive sum of image bytes (`rb1.createCheckCode`).
4. **Blocks** `0x97` (`78 97 <len8> data ck`, ≤128B) or `0xCF` (`78 CF <lenLE16=data+1> data ck`,
   ≤4096B) (`cn.java:2125,2138`). Each logical frame is re-fragmented to **20-byte GATT writes** with
   4 ms spacing by `writeOta` (`cn.java:4056`).
5. **Flow control** reply `0x06` (`parseUpdateOperation cn.java:3138`): op at `bytes[3]` —
   0=send next (`t++`), 1=resend, 2=restart (`t=0`), 3=done, 4=fail. Block index `t` is the only
   sequence number; ordering is driven entirely by the device's ACK. No CRC32 (additive checksums only).

---

## Topic 5 — Notify/response parser

Fully decoded above (Responses table). The headline is the **`0x7F` reply**: a 12-byte (`[2]==8`) or
17-byte (`[2]==13`) container used for online-status, channel-assignment and node-registration. The
parser branches on the echoed sub-opcode at `bArr[9]`: `0x9B`(155, sync/register), `0x8C`(140,
channel-assign), `0x9F`(159, add-light) — driving the multi-step add/delete state machine
(`cn.java:274–449`). `bArr[10]` is the ack/continue flag; `bArr[11]` carries the power-on state when
the reply is 17 bytes.

---

## Topic 6 — Authentication

**None at the BLE layer.** Any client that connects to service `6940…0001` and writes `0x78` frames
to char `…0002` controls the light. `sendIdentityCheck`/`sendNewIdentityCheck` (`cn.java:3412,3677`)
merely transmit the plaintext 32-bit `networkId` plus an index — no nonce, challenge, key, or
signature. The only crypto in the app is `AES/ECB/PKCS5Padding` for local account-password storage
(`LoginViewModel.java:37`), never touching BLE. "NEEWER Infinity Authenticated device" is a UI label
meaning "claimed under your account's server-assigned `networkId`" (addressing/grouping), not access
control. Frame integrity is a single additive checksum byte — error detection, not authentication.

---

## Topic 8 — Other findings

### Model → capability
- **`nj0` (`DeviceConfigInfo`) static table** keyed by integer light-type (`nj0.java:21,51`) carries
  `supportDmxMode`, `supportRGBCW`, `supportHSI`, `supportXY`, `supportGroup`, `supportDC`,
  `supportFanMode`, `haveGM`, `haveEffectPicker`, `pixelEffectClassify`, CCT min/max, `firmwareUpdateMode`.
  DMX e.g. type 96 (TL98C), 208 (HS60C PRO); pixel via `pixelEffectClassify` e.g. 217 (AP300C), 219 (AP600C).
- **type-int → marketing-name** table: `ck0.getProjectName(int)` (`ck0.java:886+`) — e.g. 8=RGB1,
  14=SL90, 50=TL120C, 28=CB200B, 85=RGB2.
- **Hard-coded predicate lists:** `ck0.isNotSupportRGB` (57,109,63,94,92,243), `ck0.isDoubleLight`
  (==61 BH40C), `ck0.isSupportRGBW` (==243 FL12C), `ck0.isSupportMusicFx` (false for 214,243,244,249,250),
  `vy4.isSupportStreamerEffect` (59,115), battery tiers in `qj0.batteryThree/Four/Five`.
- **2.4G/Infinity** is a runtime flag (`w63.is24GDevice`) set from the `NW…&XXXXXXXX` advertised name.

### Group-id (`&XXXXXXXX`) assignment
The 8-hex name suffix is the **little-endian** 32-bit `networkId`, server-assigned
(`VerifyViewModel:442`), `&FFFFFFFF` = unassigned. Written to devices via `0x8C` (channel+netid),
`0x9F` (add light + CH), `0x9B` (sync), and broadcast via `0xA3` (identity). The per-device 2.4G
channel = `sceneIndex*10` (+groupIndex) computed from list position (`i80.getDeviceChannelNum`,
`i80.java:727`).

### DMX (hardware DMX-512)
- Enable/disable: `78 CA 02 01 <EN> ck` (`setLightDmxSwicth cn.java:3808`).
- Set DMX group + start address: `78 CA 04 02 <GG> <addrHi addrLo> 00 ck` (address big-endian via
  `kq.int22ByteArray`, `setLightDmxGroupCh cn.java:3802`).
- Local play: `78 CA 04 04 <mode> <play> <speed> ck` (`playDMX cn.java:3145`).
- Query mode: `78 C9 01 <i2> ck` (`queryLightDmxMode cn.java:3271`).
- DMX **effect download** (custom colour animation, separate from streamer): per-block
  `78 CC <len> <blockIdx> <blockData> 00 ck` (`NWDMXViewModel:346`) and frame header
  `78 CB <len> 00 <sizeLo sizeHi> <size4> <…> 07 80 …` (`NWDMXViewModel:403`).

### Factory / test
`0xD8` (`sendFactoryTest`, MAC-addressed, 5 params), plus direct `0xF0`/`0xF5` test toggles
(payloads `AA`/`CC`), and `0x1D` factory reply (`isFactory cn.java:3027`).

### Gotchas for an independent implementation
- Header is `0x85` (not `0x78`) for OTA frames on `deviceClassify==6` devices.
- `networkId` is little-endian in the name suffix and in most payloads, but `setNewRGB1*`/
  `sendNewIdentityCheck` variants write only the **low 16 bits**.
- Several builders (`setMusicInfinityGradient`, `getInfinityOnlineStatus`) compute the checksum into
  `bArr` in place but the method returns the (same) array — the checksum is the **last** element.
- Pixel/streamer multi-frame sends rely on inter-frame timing (80 ms / 200 ms), not on ACKs.
- Group/Infinity commands must be sent to the **master** device; the master relays over 2.4G. A device
  is master if it is the first group member with a live BLE connection (`User.getMasterDevice`).

---

## Live validation on TL120C (2026-06-29)

Tested Streamer against a real **TL120C** (`CC:8D:BE:BB:25:B0`, ungrouped `&FFFFFFFF`):
- **`0xC0` realtime play DOES engage Streamer mode** on the TL120C: sending
  `78 C0 0A FFFFFFFF 00 <EE BR SP DR OS> ck` made the tube go **black** — it entered
  streamer mode but rendered nothing because no color blocks were uploaded. Plain `0x86`
  HSI was ignored while latched in that mode.
- **Recovery: power-cycle.** `0x81 02` then `0x81 01` exits streamer mode; direct HSI/CCT
  control returns immediately (verified off→red→green→blue).
- Neither `0x80` (version) nor `0xC4` (streamer-support query) returned a notify reply on the
  directly-connected, ungrouped TL120C. The 120 *did* push an unsolicited `0x05` battery
  reply on connect with value `0xF0` (different encoding than TL90C's 0–100 — likely a
  mains/external-power flag).
- **Conclusion:** the TL120C is a real Streamer/pixel fixture (app block table: TL120→16
  color blocks). Full per-segment control requires the `0xBF` multi-part color-block upload
  FIRST (StepThree header `78 BF <lenLo lenHi>` + EM + StepOne `DN MCh MCl TCh TCl` +
  StepTwo per fixture `DN OCN startBE16 MAC6` + 3-byte color blocks + checkSum2), THEN `0xC0`
  to play. 16 blocks ⇒ potential 16-segment addressable control of the tube.
