#!/usr/bin/env bash
# Builds testbed/: files to index and search, with known answers (see evals/evals.json).
# File names say nothing about what's inside, so a match has to come from the content.
#
#   scripts/make-testbed.sh [folder]      (default: testbed/ in the repository)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/testbed}"
rm -rf "$OUT"
mkdir -p "$OUT/Documents" "$OUT/Pictures" "$OUT/Recordings" "$OUT/Movies" "$OUT/Screenshots"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- Documents -------------------------------------------------------------------------------------------------------

cat > "$OUT/Documents/notes-0311.md" <<'EOF'
# Notes

Read a long piece about sleep this week. The main points worth keeping:

- Keep the same wake-up time every day, weekends included. The body clock (circadian rhythm) anchors on the morning,
  not on when you go to bed.
- Caffeine has a half-life of about five to six hours, so the 4 pm coffee is still half there at 10 pm.
- Bright light in the morning, dim light in the evening. Screens matter less than the overall brightness.
- If you can't fall asleep after twenty minutes, get up and read something dull in another room.

Try: no coffee after 1 pm for two weeks and see what happens.
EOF

cat > "$OUT/Documents/keuken.txt" <<'EOF'
Pannenkoeken (voor 4 personen)

250 gram bloem, een snufje zout, 2 eieren en een halve liter melk.
Doe de bloem en het zout in een kom, maak een kuiltje en breek de eieren erin. Voeg al roerend de helft van de melk
toe tot er een glad beslag ontstaat, en daarna de rest van de melk. Laat het beslag een half uur rusten.
Smelt een klontje boter in de koekenpan, schep er een flinke soeplepel beslag in en bak de pannenkoek op middelhoog
vuur tot de bovenkant droog is. Omdraaien en de andere kant nog een minuut bakken.
Lekker met stroop, poedersuiker of appel en kaneel.
EOF

cat > "$OUT/Documents/2024-05-12.md" <<'EOF'
Planning meeting, 12 May

Present: Anna, Bram, Chen, Dewi.

1. Q3 roadmap: the mobile app redesign moves to September. The web dashboard ships first.
2. Budget: marketing spend stays flat; the extra 40k goes to infrastructure (database migration).
3. Hiring: open a position for a senior frontend developer. Bram writes the job post by Friday.
4. Next meeting in two weeks, same room.
EOF

cat > "$TMP/trip.txt" <<'EOF'
Japan, November

3 Nov: flight to Tokyo Haneda, arrive 16:40. Hotel in Asakusa, near the Senso-ji temple.
4 Nov: day trip to Kamakura by train to see the Great Buddha, then the Enoshima coast. Back for dinner in Shibuya.
5 Nov: teamLab Planets in the morning, Akihabara in the afternoon.
6 Nov: shinkansen to Kyoto. Fushimi Inari at sunrise to avoid the crowds.
7 Nov: Arashiyama bamboo grove, then the monkey park.
8 Nov: Nara deer park as a day trip; fly home from Osaka Kansai on the 9th.
EOF
textutil -convert docx -output "$OUT/Documents/plan.docx" "$TMP/trip.txt"

cat > "$TMP/ticket.txt" <<'EOF'
Support ticket #88213

Customer writes: "Every time I try to check out, the payment is declined with error code 05 (do not honour). My card
works everywhere else. I tried twice on the 3rd and once on the 4th."

Notes: the bank's fraud filter flags the merchant category. Advised the customer to call the bank and approve the
transaction, or to pay by bank transfer instead.
EOF
textutil -convert rtf -output "$OUT/Documents/ticket.rtf" "$TMP/ticket.txt"

cat > "$OUT/Documents/article.html" <<'EOF'
<html><head><title>Article</title></head><body>
<h1>Why the hives went quiet</h1>
<p>Beekeepers across the region reported colonies collapsing over the winter. The likely causes are a mix of varroa
mites, a wet spring that kept foragers inside, and fewer wildflowers along the field edges.</p>
<p>Pollination of fruit trees depends on these colonies; growers are now renting hives from further away, at a higher
price. Some farmers have started sowing strips of clover and phacelia to give the bees more to forage.</p>
</body></html>
EOF

cat > "$OUT/Documents/biblioteca.txt" <<'EOF'
Horario de la biblioteca municipal

La biblioteca abre de lunes a viernes de 9:00 a 20:00 y los sábados de 10:00 a 14:00. Los domingos está cerrada.
Durante el mes de agosto cierra a las 15:00. Para renovar un préstamo, basta con llamar o hacerlo en la web.
EOF

{
  echo "date,temperature,humidity,pressure"
  for day in $(seq 1 60); do echo "2024-06-$(printf %02d $(( (day % 30) + 1 ))),$((15 + day % 9)).$((day % 10)),$((40 + day % 30)),$((1000 + day % 25))"; done
} > "$OUT/Documents/readings.txt"

# A handbook in sections: each question's answer sits under its own heading.
cat > "$OUT/Documents/handbook.md" <<'EOF'
# Team handbook

What everyone at Northwind Studio needs to know, from the first day on. Ask Mira (operations) when something here is
unclear or out of date.

## Your first week

On your first morning you get a laptop, a building pass and accounts for email, chat and the design tools. Your buddy
walks you through the projects that are running and sits with you for the first five days. By Friday you will have
shipped one small change to a real project, however small: the point is to go through the whole process once. Lunch
on the first day is on the studio.

## Expenses

Pay for work costs yourself and claim them back. Upload a photo of the receipt in the finance app within 30 days of
the purchase; claims older than that can't be paid. Train travel is always second class, flights need approval from
your lead before you book. Meals while travelling are covered up to 25 euros a day. Software subscriptions go through
Mira, not through your own card, so the studio keeps track of the licences it pays for.

## Time off

Everyone gets 25 vacation days a year, on top of the public holidays, and the days build up from your first month.
Request time off at least two weeks ahead in the planning tool, and check with your project lead when you plan more
than a week. Up to five unused days carry over to the next year; the rest lapse on 1 April. When you're ill, tell your
lead before 10:00 and take the time you need: sick days don't come out of your vacation.

## Working from home

You can work from home up to three days a week, as long as your project allows it. Core hours are 10:00 to 15:00,
when everyone is reachable on chat and video. Tuesdays are studio days for the whole team, for the weekly review and
lunch together. The studio pays a one-off 300 euros towards a desk and chair for your home office.

## Equipment

Laptops are replaced every three years, or sooner when one breaks down. Monitors, keyboards and headphones can be
ordered through Mira. Everything stays the studio's property: when you leave, it comes back on your last day.
EOF

# Records: contacts in a table, books in a list, a garden by plant, a journal by line, a backup's settings by table.
cat > "$OUT/Documents/contacts.csv" <<'EOF'
name,role,company,city,phone
Anneke Bos,general practitioner,Huisartsenpraktijk Oost,Utrecht,030-2511840
Joris Visser,dog walker,Uitlaatservice Visser,Amersfoort,06-46133770
Fatima El Idrissi,translator,Vertaalbureau De Linde,Utrecht,030-2714462
Kees Jansen,plumber,Jansen Installatietechniek,Zeist,030-6952210
Sanne de Graaf,piano teacher,,Utrecht,06-41873320
Thomas Mulder,electrician,Mulder Elektra,Bilthoven,030-2283391
Lotte Smit,veterinarian,Dierenkliniek Wittevrouwen,Utrecht,030-2717705
Ruben Koster,locksmith,Slotenmakerij Koster,Utrecht,06-29384751
Inge Willems,physiotherapist,Fysio Lombok,Utrecht,030-2943318
Noor Hendriks,hairdresser,Salon Noor,Utrecht,030-2366019
Daan Peters,car mechanic,Garage Peters,Nieuwegein,030-6033542
Eva Bakker,notary,Bakker Notarissen,Utrecht,030-2318890
Lucas de Wit,gardener,Groen & Wit,De Bilt,06-51209977
Yara Meijer,babysitter,,Utrecht,06-38817264
EOF

cat > "$OUT/Documents/books.json" <<'EOF'
[
  {
    "title": "Pride and Prejudice",
    "author": "Jane Austen",
    "year": 1813,
    "summary": "Elizabeth Bennet and the proud Mr Darcy misjudge each other until a letter changes her mind."
  },
  {
    "title": "Frankenstein",
    "author": "Mary Shelley",
    "year": 1818,
    "summary": "A young scientist gives life to a creature stitched together from corpses, then abandons it."
  },
  {
    "title": "Moby-Dick",
    "author": "Herman Melville",
    "year": 1851,
    "summary": "Ishmael signs on to the Pequod, whose captain, Ahab, hunts the white whale that took his leg."
  },
  {
    "title": "Crime and Punishment",
    "author": "Fyodor Dostoevsky",
    "year": 1866,
    "summary": "A poor student in St Petersburg kills a pawnbroker and is undone by his own conscience."
  },
  {
    "title": "Twenty Thousand Leagues Under the Seas",
    "author": "Jules Verne",
    "year": 1870,
    "summary": "Three castaways travel the oceans as prisoners aboard Nemo's electric submarine, the Nautilus."
  },
  {
    "title": "Mrs Dalloway",
    "author": "Virginia Woolf",
    "year": 1925,
    "summary": "One June day in London as Clarissa prepares a party, and a shell-shocked veteran unravels."
  },
  {
    "title": "Things Fall Apart",
    "author": "Chinua Achebe",
    "year": 1958,
    "summary": "Okonkwo, a proud wrestler and farmer in an Igbo village, sees his world change as missionaries arrive."
  },
  {
    "title": "One Hundred Years of Solitude",
    "author": "Gabriel García Márquez",
    "year": 1967,
    "summary": "Seven generations of the Buendía family in the town of Macondo, where the miraculous is ordinary."
  }
]
EOF

cat > "$OUT/Documents/garden.yaml" <<'EOF'
# The garden and the windowsills: what grows where, and how to keep it alive.
basil:
  spot: kitchen windowsill
  water: when the top of the soil feels dry
  sun: at least six hours
  notes: pinch off the flowers so it keeps making leaves
chillies:
  spot: greenhouse, on the top shelf
  water: every other morning in summer, at the roots, never on the leaves
  feed: liquid feed once a week after the first flowers open
  notes: the plants get heavy, tie them to a stick
lavender:
  spot: along the garden path
  water: rarely, it likes dry soil
  sun: full
  notes: cut back by a third after flowering in August
mint:
  spot: its own pot, or it takes over
  water: keep the soil moist
  sun: half shade is fine
strawberries:
  spot: hanging basket by the door
  water: daily when fruiting
  notes: net them against the blackbirds
EOF

cat > "$OUT/Documents/journal.jsonl" <<'EOF'
{"date": "2024-01-04", "entry": "First run of the year along the canal, slow but nice. Bought new running shoes."}
{"date": "2024-01-11", "entry": "Painted the hallway pale green. Two coats, the old colour kept showing through."}
{"date": "2024-01-19", "entry": "The boiler broke down overnight: no heating, no hot water. An engineer comes Monday."}
{"date": "2024-01-22", "entry": "Engineer replaced the pump in the boiler, warm again. Invoice to follow."}
{"date": "2024-02-02", "entry": "Dinner with Sam and Lea, made the lentil soup. Lea is moving to Lisbon in spring."}
{"date": "2024-02-14", "entry": "Started reading Things Fall Apart for the book club."}
{"date": "2024-02-27", "entry": "Rijksmuseum with my sister. The Night Watch is being restored behind glass."}
{"date": "2024-03-09", "entry": "Sowed the basil and chilli seeds on the windowsill."}
EOF

cat > "$OUT/Documents/backup.toml" <<'EOF'
# Settings for the home server's backup.
name = "home-server"
target = "/Volumes/Backup"

[sources]
folders = ["~/Pictures", "~/Documents", "~/Music"]
skip = ["*.tmp", "Caches"]

[schedule]
every = "night"
at = "03:00"
wake_the_mac = true

[retention]
# How long old backups are kept before they are deleted.
daily = 7
weekly = 4
monthly = 12

[notify]
email = "me@example.com"
on = ["failure"]
EOF

# A lease over four pages; the clause about pets is on page 3. Form feeds make the page breaks.
{
  echo "RESIDENTIAL LEASE AGREEMENT"; echo
  echo "1. Parties. This agreement is between the landlord, Northgate Properties B.V., and the tenant named below."
  echo "2. Premises. The landlord rents to the tenant the apartment at Keizersgracht 120-3, Amsterdam."
  echo "3. Term. The lease starts on 1 September and runs for twelve months, renewing monthly after that."
  printf '\f'
  echo "4. Rent. The monthly rent is EUR 1,650, due on the first day of each month by bank transfer."
  echo "5. Deposit. The tenant pays a security deposit of two months' rent, returned within 14 days of the end of"
  echo "   the lease minus the cost of any damage beyond normal wear."
  echo "6. Utilities. Gas, water and electricity are paid by the tenant; internet is included in the rent."
  printf '\f'
  echo "7. Animals. The tenant may keep one cat or one small dog only with the landlord's written consent. Any other"
  echo "   animal is not allowed. A pet deposit of EUR 300 applies, and the tenant is liable for damage the animal"
  echo "   causes to floors, doors or the garden."
  echo "8. Noise. Quiet hours are from 22:00 to 07:00."
  printf '\f'
  echo "9. Termination. Either party may end the lease after the first twelve months with one month's notice."
  echo "10. Signatures."
} > "$TMP/lease.txt"
cupsfilter "$TMP/lease.txt" > "$OUT/Documents/agreement.pdf" 2> /dev/null

{
  echo "INVOICE INV-2024-0113"; echo
  echo "Date: 14 March 2024"
  echo "Bill to: Altink Design, Utrecht"
  echo "Website maintenance, February                 EUR   480.00"
  echo "Hosting (annual)                              EUR   220.00"
  echo "Total due within 30 days                      EUR   700.00"
} > "$TMP/invoice.txt"
cupsfilter "$TMP/invoice.txt" > "$OUT/Documents/scan_0042.pdf" 2> /dev/null

# --- Pictures: macOS's own account pictures, renamed so their names give nothing away -----------------------------

PICTURES="/System/Library/Templates/Data/Library/User Pictures"
n=1001
for picture in Animals/Zebra Animals/Penguin Animals/Owl Animals/Parrot Animals/Eagle Instruments/Guitar \
  Instruments/Piano Instruments/Drum Sports/Tennis Sports/Basketball Nature/Cactus Nature/Lightning Nature/Snowflake \
  Flowers/Sunflower Flowers/Lotus Fun/Fortune\ Cookie; do
  cp "$PICTURES/$picture.heic" "$OUT/Pictures/IMG_$n.heic"
  echo "IMG_$n.heic $picture" >> "$TMP/pictures.txt"
  n=$((n + 1))
done
cp "$ROOT/tests/reference/photo.jpg" "$OUT/Pictures/IMG_2001.jpg"   # green hills, a tree, the sun

# --- Screenshots (named as macOS names them) --------------------------------------------------------------------------

RENDER=(swift "$ROOT/scripts/testbed/render.swift")
"${RENDER[@]}" screenshot "$OUT/Screenshots/Screenshot 2024-06-03 at 14.02.11.png" "Checkout" \
  "Payment failed" "Your card was declined (error 05: do not honour)." "Try another card or pay by bank transfer." \
  "Order total: EUR 84.95"
"${RENDER[@]}" screenshot "$OUT/Screenshots/Screenshot 2024-06-05 at 09.47.30.png" "Terminal — zsh" \
  "$ python train.py" "Traceback (most recent call last):" "  File \"train.py\", line 3, in <module>" \
  "ModuleNotFoundError: No module named 'torch'"
"${RENDER[@]}" screenshot "$OUT/Screenshots/Screenshot 2024-06-07 at 18.20.05.png" "Calendar" \
  "Thursday 13 June" "09:00  Stand-up" "14:30  Dentist, Dr. Verhoeven, Oudegracht 12" "19:00  Football training"

# --- A scanned page: a picture of text, no text layer -----------------------------------------------------------------

"${RENDER[@]}" scan "$OUT/Documents/scan_0007.pdf" "Warranty certificate" \
  "This washing machine is covered by a five-year warranty" "from the date of purchase. The warranty covers the" \
  "motor, the drum and the electronics. It does not cover" "damage from limescale or misuse. Keep this certificate" \
  "together with the receipt."

# --- Recordings: spoken with macOS's voices ---------------------------------------------------------------------------

say -o "$TMP/part1.aiff" "Welcome back to the show. Today we start with coffee. Coffee was first brewed in Yemen in \
the fifteenth century, and it spread through the Ottoman empire to Europe. The first coffee house in London opened in \
sixteen fifty two. Traders and writers met there to talk business and read the news, so much so that people called \
them penny universities, because for the price of a cup you could listen to a lecture's worth of conversation. Over \
the next centuries coffee became a global crop, grown in Brazil, Vietnam, Colombia and Ethiopia."
say -o "$TMP/part2.aiff" "Let's move on to sleep, which is what most of you wrote in about. Adults need seven to nine \
hours of sleep. The most important habit is waking up at the same time every day, even on weekends, because the body \
clock anchors on the morning. Avoid caffeine in the afternoon, keep the bedroom cool and dark, and if you lie awake \
for more than twenty minutes, get up and read something boring until you feel sleepy again. Naps are fine, but keep \
them short and early in the afternoon."
say -o "$TMP/part3.aiff" "Finally, the garden. If you grow tomatoes on a balcony, choose a large pot, at least twenty \
litres, and give them full sun. Water deeply twice a week rather than a little every day. Pinch out the side shoots \
on cordon varieties, and feed them with a potash rich fertiliser once the first flowers appear. That's all for today. \
Thanks for listening, and see you next week."
ffmpeg -loglevel error -i "$TMP/part1.aiff" -i "$TMP/part2.aiff" -i "$TMP/part3.aiff" \
  -filter_complex "[0:a][1:a][2:a]concat=n=3:v=0:a=1" -c:a aac -b:a 96k "$OUT/Recordings/episode-12.m4a"
say -o "$TMP/memo.aiff" "Note to self. On Saturday buy milk, eggs and flour for the pancakes, and pick up the \
bicycle from the repair shop before five."
ffmpeg -loglevel error -i "$TMP/memo.aiff" -c:a aac -b:a 64k "$OUT/Recordings/Recording 0003.m4a"
say -v "Eddy (Spanish (Spain))" -o "$TMP/es.aiff" "Recordatorio: la reunión con el arquitecto es el martes a las \
diez. Hay que llevar los planos de la cocina y el presupuesto de la reforma."
ffmpeg -loglevel error -i "$TMP/es.aiff" "$OUT/Recordings/nota-voz.mp3"

# --- A video: three shots (zebra, penguin, guitar) with a voice over --------------------------------------------------

for animal in Zebra Penguin; do sips -s format png "$PICTURES/Animals/$animal.heic" --out "$TMP/$animal.png" > /dev/null; done
sips -s format png "$PICTURES/Instruments/Guitar.heic" --out "$TMP/Guitar.png" > /dev/null
say -o "$TMP/voice.aiff" "This is a short film about animals and music. First, a zebra on the savanna. Then a penguin \
from the cold south. And to end, a guitar, because every film needs a soundtrack."
ffmpeg -loglevel error -loop 1 -t 12 -i "$TMP/Zebra.png" -loop 1 -t 12 -i "$TMP/Penguin.png" \
  -loop 1 -t 12 -i "$TMP/Guitar.png" -i "$TMP/voice.aiff" \
  -filter_complex "[0:v]scale=1280:720:force_original_aspect_ratio=decrease,pad=1280:720:(ow-iw)/2:(oh-ih)/2,setsar=1[a];\
[1:v]scale=1280:720:force_original_aspect_ratio=decrease,pad=1280:720:(ow-iw)/2:(oh-ih)/2,setsar=1[b];\
[2:v]scale=1280:720:force_original_aspect_ratio=decrease,pad=1280:720:(ow-iw)/2:(oh-ih)/2,setsar=1[c];\
[a][b][c]concat=n=3:v=1:a=0,format=yuv420p[v];[3:a]apad[s]" \
  -map "[v]" -map "[s]" -c:v libx264 -r 24 -c:a aac -t 36 "$OUT/Movies/clip_0001.mp4"

# --- A code project (only read with "Read Source Code" on) ------------------------------------------------------------

P="$OUT/Projects/weather-app"
mkdir -p "$P/.git" "$P/src" "$P/notebooks"
echo '{ "name": "weather-app", "version": "1.0.0" }' > "$P/package.json"
cat > "$P/src/http.ts" <<'EOF2'
import { setTimeout as sleep } from "node:timers/promises";

export class HttpError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

/** Calls fn until it succeeds, waiting longer after each failure (200 ms, 400 ms, 800 ms…). */
export async function withRetries<T>(fn: () => Promise<T>, attempts = 5): Promise<T> {
  let delay = 200;
  for (let attempt = 1; ; attempt++) {
    try {
      return await fn();
    } catch (error) {
      if (attempt >= attempts) throw error;
      await sleep(delay);
      delay *= 2;
    }
  }
}

export async function getJson(url: string): Promise<unknown> {
  const response = await fetch(url);
  if (!response.ok) throw new HttpError(response.status, `GET ${url} failed`);
  return response.json();
}
EOF2
cat > "$P/src/auth.ts" <<'EOF2'
let accessToken: string | undefined;
let expiresAt = 0;

export async function refreshAccessToken(refreshToken: string): Promise<string> {
  const response = await fetch("https://auth.example.com/token", {
    method: "POST",
    body: new URLSearchParams({ grant_type: "refresh_token", refresh_token: refreshToken }),
  });
  const body = (await response.json()) as { access_token: string; expires_in: number };
  accessToken = body.access_token;
  expiresAt = Date.now() + body.expires_in * 1000;
  return accessToken;
}

export function isExpired(): boolean {
  return !accessToken || Date.now() > expiresAt - 30_000;
}
EOF2
cat > "$P/src/units.py" <<'EOF2'
def celsius_to_fahrenheit(celsius):
    return celsius * 9 / 5 + 32


def wind_chill(temperature_c, wind_speed_kmh):
    """The temperature it feels like in the wind (the North American formula)."""
    v = wind_speed_kmh ** 0.16
    return 13.12 + 0.6215 * temperature_c - 11.37 * v + 0.3965 * temperature_c * v
EOF2
python3 - "$P/notebooks/explore.ipynb" <<'EOF2'
import json, sys
cells = [
    {"cell_type": "markdown", "metadata": {}, "source": ["# Exploring the station data"]},
    {"cell_type": "code", "metadata": {}, "execution_count": 1, "outputs": [], "source": [
        "import pandas as pd\n", "df = pd.read_csv('stations.csv')\n", "df.head()"]},
    {"cell_type": "code", "metadata": {}, "execution_count": 2, "outputs": [], "source": [
        "counts = df['label'].value_counts()\n", "counts.plot(kind='bar', title='Examples per class')"]},
]
json.dump({"cells": cells, "metadata": {}, "nbformat": 4, "nbformat_minor": 5}, open(sys.argv[1], "w"), indent=1)
EOF2

# A class longer than a passage may be, cut at its methods; a long function with blank lines in it, kept whole.
cat > "$P/src/forecast.py" <<'EOF2'
"""Forecasts from the station readings: daily summaries, and which way the weather is going."""

from dataclasses import dataclass
from statistics import mean


@dataclass
class Reading:
    day: str
    high_c: float
    low_c: float
    rain_mm: float
    wind_kmh: float


class Week:
    """Seven days of readings from one station, and what they say."""

    def __init__(self, station, readings):
        self.station = station
        self.readings = sorted(readings, key=lambda reading: reading.day)

    def average_high(self):
        """The mean of the daily maximum temperatures, in degrees Celsius."""
        return mean(reading.high_c for reading in self.readings)

    def warmest_day(self):
        return max(self.readings, key=lambda reading: reading.high_c).day

    def frost_nights(self):
        """The nights the temperature dropped below zero."""
        return [reading.day for reading in self.readings if reading.low_c < 0]

    def total_rain(self):
        return sum(reading.rain_mm for reading in self.readings)

    def wet_days(self, threshold_mm=2.0):
        """Days with enough rain to need an umbrella."""
        return [reading.day for reading in self.readings if reading.rain_mm >= threshold_mm]

    def windiest_day(self):
        return max(self.readings, key=lambda reading: reading.wind_kmh).day

    def trend(self):
        """Whether it's getting warmer or colder: the slope of a straight line through the highs, in degrees a day."""
        days = len(self.readings)
        x_mean = (days - 1) / 2
        y_mean = self.average_high()
        top = sum((x - x_mean) * (reading.high_c - y_mean) for x, reading in enumerate(self.readings))
        bottom = sum((x - x_mean) ** 2 for x in range(days))
        return top / bottom if bottom else 0.0

    def summary(self):
        """One line for the dashboard."""
        direction = "warming" if self.trend() > 0 else "cooling"
        return (
            f"{self.station}: average high {self.average_high():.1f} °C, {len(self.frost_nights())} frost nights, "
            f"{self.total_rain():.0f} mm of rain, {direction}"
        )


def load_week(station, rows):
    """A week from CSV rows of day, high, low, rain and wind."""
    readings = [Reading(day, float(high), float(low), float(rain), float(wind)) for day, high, low, rain, wind in rows]
    return Week(station, readings)
EOF2
cat > "$P/src/sync.ts" <<'EOF2'
import { getJson } from "./http";

export interface Reading {
  station: string;
  takenAt: number;
  celsius: number;
}

const delivered = new Set<string>();

/**
 * Sends the readings that were saved while the device was offline, oldest first. One the server already has (the
 * same station and time) is dropped; when the server can't be reached, the rest wait for the next try.
 */
export async function sendSavedReadings(saved: Reading[], post: (reading: Reading) => Promise<Response>) {
  const oldestFirst = [...saved].sort((a, b) => a.takenAt - b.takenAt);
  let sent = 0;

  for (const reading of oldestFirst) {
    const key = `${reading.station}@${reading.takenAt}`;
    if (delivered.has(key)) {
      saved.splice(saved.indexOf(reading), 1);
      continue;
    }

    let response: Response;
    try {
      response = await post(reading);
    } catch {
      // Offline again: keep the rest for later.
      break;
    }

    if (response.status === 409 || response.ok) {
      // 409: the server had it already.
      delivered.add(key);
      saved.splice(saved.indexOf(reading), 1);
      sent += 1;
    }
  }

  return sent;
}

export async function latestReading(station: string): Promise<Reading> {
  return (await getJson(`/stations/${station}/latest`)) as Reading;
}
EOF2

# A second project: a map's tile cache in Rust, its impl block longer than a passage may be.
T="$OUT/Projects/tiles"
mkdir -p "$T/.git" "$T/src"
printf '[package]\nname = "tiles"\nversion = "0.1.0"\nedition = "2024"\n' > "$T/Cargo.toml"
cat > "$T/src/cache.rs" <<'EOF2'
//! A cache of map tiles in memory, so panning back and forth doesn't fetch them again.

use std::collections::HashMap;

/// A tile's place: zoom level, column and row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileId {
    pub zoom: u8,
    pub x: u32,
    pub y: u32,
}

/// Holds up to `capacity` tiles; when it's full, the tile used longest ago makes room.
pub struct TileCache {
    capacity: usize,
    tiles: HashMap<TileId, (Vec<u8>, u64)>,
    clock: u64,
    hits: u64,
    misses: u64,
}

impl TileCache {
    pub fn new(capacity: usize) -> Self {
        Self { capacity, tiles: HashMap::new(), clock: 0, hits: 0, misses: 0 }
    }

    /// The tile's image, if it's here; it counts as just used.
    pub fn get(&mut self, id: TileId) -> Option<&[u8]> {
        self.clock += 1;
        match self.tiles.get_mut(&id) {
            Some((image, used)) => {
                *used = self.clock;
                self.hits += 1;
                Some(image)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// Keeps a tile. When the cache is full, the least recently used tile is dropped first.
    pub fn insert(&mut self, id: TileId, image: Vec<u8>) {
        self.clock += 1;
        if self.tiles.len() >= self.capacity && !self.tiles.contains_key(&id) {
            let oldest = self.tiles.iter().min_by_key(|(_, (_, used))| *used).map(|(id, _)| *id);
            if let Some(oldest) = oldest {
                self.tiles.remove(&oldest);
            }
        }
        self.tiles.insert(id, (image, self.clock));
    }

    /// The tiles around one, at the same zoom, that aren't here yet: what to fetch before the user pans to them.
    pub fn missing_neighbours(&self, id: TileId) -> Vec<TileId> {
        let mut missing = Vec::new();
        for dx in -1i64..=1 {
            for dy in -1i64..=1 {
                let (x, y) = (id.x as i64 + dx, id.y as i64 + dy);
                if (dx, dy) == (0, 0) || x < 0 || y < 0 {
                    continue;
                }
                let neighbour = TileId { zoom: id.zoom, x: x as u32, y: y as u32 };
                if !self.tiles.contains_key(&neighbour) {
                    missing.push(neighbour);
                }
            }
        }
        missing
    }

    /// Drops every tile of a zoom level, after the map's style changed there.
    pub fn clear_zoom(&mut self, zoom: u8) {
        self.tiles.retain(|id, _| id.zoom != zoom);
    }

    /// The share of lookups that found their tile.
    pub fn hit_rate(&self) -> f64 {
        let lookups = self.hits + self.misses;
        if lookups == 0 { 0.0 } else { self.hits as f64 / lookups as f64 }
    }
}
EOF2

cp "$TMP/pictures.txt" "$OUT/.pictures.txt"   # which IMG is which (hidden: not indexed)
echo "testbed: $OUT"
find "$OUT" -type f -not -name '.*' | sort
