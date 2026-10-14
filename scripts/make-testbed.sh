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

cp "$TMP/pictures.txt" "$OUT/.pictures.txt"   # which IMG is which (hidden: not indexed)
echo "testbed: $OUT"
find "$OUT" -type f -not -name '.*' | sort
