# Niederschlagsradar-Daten (DWD) — Recherche

Ziel: eigene, optisch ansprechende Regenradar-Karte statt der bestehenden Apps.

Kernbefund: Die Radardaten, auf denen WetterOnline & Co. aufsetzen, kommen vom DWD
und sind seit 2017 **Open Data** — kostenlos, kommerziell nutzbar, kein API-Key,
keine Registrierung. Lizenz: GeoNutzV, Auflage ist nur die Quellenangabe
„Datenbasis: Deutscher Wetterdienst".

---

## 1. Welches Produkt?

Entscheidend ist die Unterscheidung **Niederschlag (mm)** vs. **Reflektivität (dBZ)**
und **angeeicht** vs. **nur Radar**.

### Nowcast — jetzt bis +2 h

| Produkt | Inhalt | Auflösung | Update |
|---|---|---|---|
| **RV** | Niederschlag in mm, inkl. Vorhersage bis +120 min in 5-Min-Schritten | 1 km | alle 5 min |
| WN | Reflektivität (dBZ), ebenfalls mit +2 h Vorhersage | 1 km | alle 5 min |

→ **RV nehmen.** WN ist Reflektivität, nicht Niederschlagsmenge.
RV ist *nicht* an Bodenstationen angeeicht — für „kommt gleich Regen?" völlig
ausreichend, für tatsächlich gefallene Millimeter nicht.

### Vergangenheit / echte Mengen

Unter `https://opendata.dwd.de/weather/radar/radolan/`:

| Produkt | Auflösung | Aneichung |
|---|---|---|
| `ry/` | 5 min | nur Radar, qualitätsgeprüft |
| `yw/` | 5 min | mit Stationsdaten angeeicht |
| `rw/` | 1 h | mit Stationsdaten angeeicht |
| `sf/` | Tagessumme | mit Stationsdaten angeeicht |

Radar allein schätzt die Menge nur über die Reflektivität und liegt schnell 30–50 %
daneben. Für „wie viel ist gefallen?" die angeeichten Produkte (yw/rw/sf) nehmen.

Für lange Zeitreihen ab 2001 gibt es zusätzlich **RADKLIM** (reprozessierte
Klimareihe) — noch nicht weiter recherchiert.

---

## 2. Zugangswege

### A) WMS — fertige Kacheln, schnellster Weg

```
https://maps.dwd.de/geoserver/wms
```

Anonymer Zugriff, 1 Workspace, 176 Layer.

- `dwd:Niederschlagsradar` — Alias fürs RV-Produkt, 1 km, alle 5 min, in mm/h
- Radarkomposit WN — 1 km, alle 5 min, inkl. Vorhersage +2 h in 5-Min-Schritten

Exakte Layernamen und verfügbare `TIME`-Zeitschritte aus dem `GetCapabilities` ziehen.

Legenden:
```
https://maps.dwd.de/geoserver/wms?REQUEST=GetLegendGraphic&version=1.3&format=image/png&width=20&height=20&layer=[LAYERNAME]
```

**Nachteil:** man bekommt die DWD-Farbskala — also genau den Look, vor dem man wegläuft.

### B) Bright Sky — JSON-API mit Pixelwerten

```
https://api.brightsky.dev/radar?lat=48.5&lon=9.2&distance=1000&format=plain
```

Kein API-Key, „free-to-use for all purposes". Open Source, selbst hostbar.
Liefert das RV-Produkt als Pixel-Array.

**Einheit von `precipitation_5`: 0,01 mm / 5 min** (Integer).

Query-Parameter:

| Name | Bedeutung | Default |
|---|---|---|
| `bbox` | Bounding Box (top, left, bottom, right) **in Pixeln**, Kanten inklusive | volles 1200×1100-Gitter |
| `distance` | Alternative zur bbox, zusammen mit `lat`/`lon` | 200000 (Meter) |
| `lat` / `lon` | Dezimalgrad | — |
| `date` | Zeitstempel des ersten Records, ISO 8601 | 1 h vor letzter Messung |
| `last_date` | Zeitstempel des letzten Records, ISO 8601 | 2 h nach `date` |
| `format` | `compressed` / `bytes` / `plain` | `compressed` |
| `tz` | Zeitzone der Zeitstempel, tz-Database-Name | UTC |

Antwortstruktur: `radar` (Liste mit `timestamp`, `source`, `precipitation_5`),
`geometry`, `bbox`, `latlon_position`.

- `source` sieht aus wie `RADARCOMP::RV::<timestamp>` — bei Vorhersage-Schritten
  bleibt der Source-Zeitstempel auf dem letzten Messlauf stehen, daran erkennt man
  Messung vs. Nowcast.
- `latlon_position` (x/y) gibt an, welches Pixel im Array genau die angefragte
  Koordinate ist → punktgenaues Auslesen ohne eigene Projektion.

Damit lässt sich auch „X mm in der nächsten Stunde" direkt bauen: Zeitschritte
aufsummieren.

### C) Rohdaten vom DWD

```
https://opendata.dwd.de/weather/radar/composite/rv/composite_rv_LATEST.tar
```

(Bis Herbst 2026: `DE1200_RV_LATEST.tar.bz2` im RADOLAN-Binärformat; seitdem ODIM_H5,
gleiches Gitter. Der alte Name liefert 404.)

Verzeichnis `https://opendata.dwd.de/weather/radar/composite/` enthält u. a.:
`dmax/ hg/ hx/ hymecng/ pg/ rs/ rv/ vii/ wn/`

Alle 5 Minuten neu, ein tar mit den Zeitschritten von jetzt bis +2 h.
RADOLAN-Binärformat auf dem 1200×1100-Gitter. Lesbar mit **wradlib** (Python).

Nur nötig bei eigener Verarbeitung — sonst tut es Bright Sky.

---

## 3. Rendering

Das Raster ist **1100 × 1200 Pixel à 1 km**, ein Niederschlagswert pro Pixel pro
5-Minuten-Schritt. Genau das, was die Apps rendern.

### Eigene Farbskala

Bright Sky mit `format=bytes` oder `compressed` liefert das rohe Array. Durch eigene
Colormap auf ein `<canvas>`, dann als Layer über die Karte. Volle Kontrolle über
Farbverlauf, Transparenz bei Nieselregen, weiche Kanten statt Klötzchen, Blending
mit dem Basemap-Style.

```js
// precipitation_5 ist 0.01 mm / 5 min  →  mm/h = wert * 0.12
const mmh = wert * 0.12
```

Farbskala über mm/h definieren, nicht über den Rohwert.

### Fallstrick: Georeferenzierung

Das RADOLAN-Gitter ist **polarstereographisch und gegenüber Nord verdreht**
(deckungsgleich nur bei 10° Ost). Ein als lat/lon-Rechteck platziertes Bild verrutscht
an den Rändern.

Lösung: Bright Sky liefert in `geometry` die vier Eckkoordinaten des Ausschnitts,
und MapLibres `ImageSource` nimmt genau vier Eckpunkte als Quad entgegen. Damit passt
es ohne eigene Reprojektion.

### Datenmenge

Vollbild = 1,32 Mio. Pixel × 25 Zeitschritte. Fürs Handy mit `distance` oder `bbox`
einen Ausschnitt um den Nutzer schneiden → ein paar hundert KB pro Abruf, alle 5 min neu.

### Zielbild

Dunkle Vektorkarte (MapLibre), weicher Regen-Layer drüber, Zeitschieber über die
5-Minuten-Schritte von −1 h bis +2 h.

---

## 4. Verworfene Apps (und warum)

| App | Problem |
|---|---|
| Ventusky | deutlich ungenauer als das WetterOnline-Regenradar |
| RainViewer | kein Forecast — `nowcast`-Array der öffentlichen API ist leer (13 Vergangenheits-Frames à 10 min, null Vorhersage) |
| Windy | zu unübersichtlich, nichts zu erkennen |
| Kachelmannwetter | taugt nicht |

---

## Quellen

- DWD Populäre Radarlayer — https://www.dwd.de/DE/leistungen/radarprodukte/radarlayer.html
- DWD GeoServer — https://maps.dwd.de/geoserver/web/
- Bright Sky OpenAPI-Spec — https://api.brightsky.dev/openapi.json
- DWD Open Data RV-Komposit — https://opendata.dwd.de/weather/radar/composite/rv/
- DWD Open Data RADOLAN — https://opendata.dwd.de/weather/radar/radolan/

Stand: 2026-08-17

---

## Nachtrag: gemessene Korrekturen aus der Umsetzung

Beim Bau der App haben sich drei Annahmen oben als falsch bzw. unvollständig
herausgestellt. Alles unten ist nachgemessen, nicht aus Doku abgeschrieben.

### Kein europaweites Radar verfügbar

Das RV-Gitter ist 1100 × 1200 km, echte Radardaten liegen darin auf **702 585 km²**,
Hüllrechteck lon 4,41–16,10 / lat 46,52–55,52. Also Deutschland plus 100–250 km
Grenzstreifen — nicht Europa.

`opendata.dwd.de/weather/radar/composite/` enthält nur `dmax hg hx hymecng pg rs rv wn`,
alles deutsche Komposite. Der GeoServer verweist in der Abstract von
`Radar_eucom_zeros` auf `Radar_eucom-zhproduct_1x1km_eu` (EUMETNET OPERA), aber ein
`GetMap` darauf liefert anonym eine ServiceException — nur die No-Data-Maske ist
öffentlich. OPERA ist lizenzbeschränkt.

### Open-Meteo zählt pro Koordinate, nicht pro Request

Die Preisseite legt nahe, dass nur Variablenzahl und Zeitraum gewichtet werden. Test:

- 1 POST mit 1000 Koordinaten → 200, Antwort 576 KB in 0,38 s.
- Zweiter identischer Request 1 s später → **429**.
- 35 Chunks à 1000 Punkte → ab dem vierten 429, Text „Hourly API request limit exceeded".

Effektives Limit also **600 Koordinaten/min, 5 000/h, 10 000/Tag**. Ein festes
Europaraster mit 0,3° × 0,18° wären 34 768 Punkte *pro Aktualisierung* — unmöglich.
Konsequenz: Europa-Ebene wird pro Viewport mit festem Punktbudget gerendert und
gecacht, und entfällt ganz, solange der Ausschnitt in der Radarabdeckung liegt.

`minutely_15` ist europaweit verfügbar (in Madrid, Athen, Kiew, Reykjavík, Lissabon
geprüft), außerhalb der 15-Minuten-Modelle aus Stundenwerten interpoliert.

### ICON-EU als Alternative: GRIB2 Template 42

`icon-eu_europe_regular-lat-lon_single-level_*_TOT_PREC.grib2.bz2` ist 1377 × 657 auf
0,0625° (lon 336,5–62,5 / lat 29,5–70,5) — schön, aber Section 5 meldet **Data
Representation Template 42**, also CCSDS/AEC-Kompression. Kein simple packing, damit
kein 100-Zeilen-Decoder; es bräuchte eccodes oder einen eigenen AEC-Decoder. Dafür
nur stündlich und bei 3-stündigem Lauf plus Bereitstellungszeit bis zu 5 h alt.

### RADOLAN-Details, die man beim Dekodieren braucht

- Header endet mit `ETX` (0x03), hier 189 Bytes, dann 1100 × 1200 × `u16` little-endian.
- **Zeilen laufen Süd → Nord**, Spalten West → Ost (gegen Bright Sky verifiziert, das
  intern Nord → Süd indiziert).
- Bit 13 (`0x2000`) = No Data, Füllwert `0x29C4`. Nutzwert ist `wert & 0x0FFF`.
- `PR E-02` = 0,01 mm pro `INT`-Intervall (5 min) → mm/h = Wert × 0,12.
- DE1200-Gitter, gegen Bright Skys `latlon_position` für fünf Städte auf < 1 m gefittet:
  polarstereographisch, WGS84, `lat_ts` 60° N, `lon_0` 10° O,
  x der Spaltenmitte 0 = −543 196,835 m, y der nördlichsten Zeilenmitte = −3 622 588,860 m
  (beide polzentriert), 1000 m Raster.

### Konsequenz für die Umsetzung

Nach diesen Befunden ist die App auf das RV-Komposit beschränkt: bestes verfügbares
Radar dort, wo es existiert, und nichts dort, wo es das nicht tut. Eine
Modelldaten-Ebene für den Rest Europas war gebaut, aber ~20 km grob, quotenpflichtig
und genau dort unsichtbar, wo das Radar hinreicht — der Aufwand stand in keinem
Verhältnis. Details in der README.
