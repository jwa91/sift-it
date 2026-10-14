#!/usr/bin/env bash
# Builds a folder to show sift-it with: a few hundred photos, sounds, recordings, music, videos, PDFs, books and notes
# (about 370 MB), from public collections: Wikimedia Commons, COCO, ESC-50, NASA, the Prelinger Archives, Wikipedia
# and Project Gutenberg. Files get the names a phone or a Downloads folder gives them (IMG_1042.jpg,
# New Recording 7.m4a, download (3).pdf), so whatever sift-it finds, it found by what's in them. CREDITS.csv (which
# sift-it doesn't index) says where each file came from, who made it, and its licence: they're for showing sift-it,
# not for passing on.
#
#   scripts/make-demo.sh [folder]      (default: ~/sift-it-demo)
#
# Running it again fetches only what's missing or changed: each file keeps the url it came from (in an extended
# attribute). Needs curl, jq and ffmpeg.
set -euo pipefail

OUT="${1:-$HOME/sift-it-demo}"
UA="sift-it-demo (https://github.com/jwa91/sift-it)"
for tool in curl jq ffmpeg textutil md5; do
  command -v "$tool" > /dev/null || {
    echo "make-demo: $tool missing" >&2
    exit 1
  }
done
mkdir -p "$OUT"/{Photos,Recordings,Music,Videos,Documents,Books}
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
CREDITS="$TMP/credits.csv"
echo 'file,collection,title,author,licence,source' > "$CREDITS"

get() { curl -fsSL --retry 3 --retry-delay 2 -A "$UA" "$@"; }

# A list that can't be had stops the run: going on without it would shift the names of the files after it.
fail() {
  echo "make-demo: $*; nothing was named or fetched from this list, run it again" >&2
  exit 1
}

# A manifest row: url, extension, collection, title, author, licence, page; tab-separated, never empty (read with
# IFS=tab would merge the tabs around an empty field).
row() {
  local out="" field
  for field in "$@"; do
    field="$(printf '%s' "$field" | tr '\t\n' '  ')"
    out+="${field:-–}"$'\t'
  done
  printf '%s\n' "${out%$'\t'}"
}

credit() {
  local out="" field
  for field in "$@"; do
    out+="\"${field//\"/\"\"}\","
  done
  printf '%s\n' "${out%,}" >> "$CREDITS"
}

SOURCE=dev.sift-it.demo.source

# fetch <url> <file>: downloads it unless it's there from the same url. With trim: in front of the url, a video is kept
# to its first 3 minutes; with remux:, it's put in a proper MP4 container (some of NASA's .m4a files are bare AAC).
fetch() {
  local url="$1" file="$2" failed=false
  [[ -s "$file" && "$(xattr -p "$SOURCE" "$file" 2> /dev/null)" == "$url" ]] && return 0
  case "$url" in
    trim:*) ffmpeg -nostdin -loglevel error -y -t 180 -i "${url#trim:}" -c copy -movflags +faststart "$TMP/part.mp4" \
      && mv "$TMP/part.mp4" "$file" || failed=true ;;
    remux:*) ffmpeg -nostdin -loglevel error -y -i "${url#remux:}" -c copy -movflags +faststart -f mp4 "$TMP/part" \
      && mv "$TMP/part" "$file" || failed=true ;;
    *) get -o "$TMP/part" "$url" && mv "$TMP/part" "$file" || failed=true ;;
  esac
  if $failed; then
    echo "  couldn't fetch $url" >&2
    return 1
  fi
  xattr -w "$SOURCE" "$url" "$file"
}

# place <manifest> <folder> <name format> <first number>: names the manifest's files in an order that mixes the
# collections (the same order each run, so names stay put), fetches what's missing, and credits each. Files an earlier
# run put in the folder that this one didn't name go.
place() {
  local manifest="$1" folder="$2" format="$3" n="$4" name key file
  local url ext collection title author licence page
  : > "$TMP/named"
  while IFS=$'\t' read -r _ url ext collection title author licence page; do
    # shellcheck disable=SC2059
    name="$(printf "$format" "$n").$ext"
    n=$((n + 1))
    echo "$name" >> "$TMP/named"
    if fetch "$url" "$OUT/$folder/$name"; then
      credit "$folder/$name" "$collection" "$title" "$author" "$licence" "$page"
    fi
  done < <(while IFS= read -r line; do
    key="${line%%$'\t'*}" key="${key#trim:}" key="${key#remux:}"
    printf '%s\t%s\n' "$(printf '%s' "$key" | md5)" "$line"
  done < "$manifest" | sort)
  for file in "$OUT/$folder"/*; do
    [[ -f "$file" ]] && xattr -p "$SOURCE" "$file" > /dev/null 2>&1 && ! grep -qxF "${file##*/}" "$TMP/named" \
      && rm "$file"
  done
  echo "  $folder: $(find "$OUT/$folder" -type f | wc -l | tr -d ' ') files"
}

# --- Photos: Wikimedia Commons featured pictures, and everyday ones from COCO ----------------------------------------

echo "Photos"
commons=(
  "food:20" "dogs:10" "cats:10" "Rodentia:5" "Cetacea:5" "birds in flight:15" "Passeriformes:8" "Galliformes:5"
  "flowers:15" "landscapes:20" "mountains:12" "beaches:12" "waterfalls:10" "sunsets:12" "clouds:8" "snow:8"
  "lightning:3" "volcanism:5" "bridges:10" "lighthouses:8" "architecture:10" "automobiles:10" "rail vehicles:10"
  "bicycles:5" "motorcycles:5" "ships:8" "aircraft:5" "children:10" "people:12" "musicians:5" "surfing:5"
  "cycling:5" "tennis:4" "association football:5" "winter sports:5" "chess:3" "musical instruments:8" "posters:6"
  "machines:6"
)
for entry in "${commons[@]}"; do
  category="Category:Featured pictures of ${entry%:*}"
  get -G "https://commons.wikimedia.org/w/api.php" \
    --data-urlencode "gcmtitle=$category" \
    -d "action=query&format=json&generator=categorymembers&gcmtype=file&gcmlimit=50&prop=imageinfo" \
    -d "iiprop=url|mime|extmetadata&iiurlwidth=960&iiextmetadatafilter=Artist|LicenseShortName" \
    | jq -r --argjson n "${entry##*:}" '
        [(.query.pages // {})[] | select(.imageinfo) | .imageinfo[0] as $i
          | ($i.thumburl | sub("\\?.*$"; "")) as $thumb
          | select($thumb | test("\\.jpe?g$"; "i"))
          | { title: (.title | ltrimstr("File:") | sub("\\.[^.]+$"; "")), url: $thumb, page: $i.descriptionurl,
              author: ($i.extmetadata.Artist.value // "" | gsub("<[^>]*>"; "") | gsub("\\s+"; " ")),
              licence: ($i.extmetadata.LicenseShortName.value // "") }]
        | sort_by(.title) | length as $len | ([$n, $len] | min) as $take
        | .[range(0; $take) | . * $len / $take | floor]
        | [.url, "jpg", "Wikimedia Commons, featured picture", .title, .author, .licence, .page]
        | map(if . == "" then "–" else gsub("\t"; " ") end) | @tsv' \
    > "$TMP/category.tsv" && [[ -s "$TMP/category.tsv" ]] || fail "couldn't list $category"
  cat "$TMP/category.tsv" >> "$TMP/photos.raw"
done
get "http://images.cocodataset.org/?list-type=2&prefix=val2017/&max-keys=1000" \
  | grep -o '<Key>[^<]*</Key>' | sed 's/<[^>]*>//g' | awk 'NR % 10 == 1' \
  | while read -r key; do
    row "http://images.cocodataset.org/$key" jpg "COCO 2017 (photos from Flickr)" "${key##*/}" "" \
      "Creative Commons, per photo (see cocodataset.org)" "https://cocodataset.org"
  done >> "$TMP/photos.raw" || fail "couldn't list COCO's photos"
awk -F'\t' '!seen[$1]++' "$TMP/photos.raw" > "$TMP/photos.tsv"
place "$TMP/photos.tsv" Photos "IMG_%04d" 1001

# --- Recordings: everyday sounds (ESC-50) and long talks (NASA's podcast) --------------------------------------------

echo "Recordings"
sounds="dog rooster cow frog cat hen crow chirping_birds sea_waves crackling_fire rain thunderstorm crickets crying_baby
sneezing clapping laughing snoring footsteps brushing_teeth door_wood_knock keyboard_typing vacuum_cleaner clock_alarm
church_bells siren car_horn train airplane fireworks glass_breaking toilet_flush"
esc="https://raw.githubusercontent.com/karolpiczak/ESC-50/master"
get "$esc/meta/esc50.csv" \
  | awk -F, -v want="$(printf '%s' "$sounds" | tr '\n' ' ')" 'BEGIN { n = split(want, w, " "); for (i = 1; i <= n; i++) ok[w[i]] = 1 }
      NR > 1 && ok[$4] && !done[$4]++ { print $1 "\t" $4 }' \
  | while IFS=$'\t' read -r file sound; do
    row "$esc/audio/$file" wav "ESC-50 (clips from freesound.org)" "${sound//_/ }" "" "CC BY-NC 3.0" \
      "https://github.com/karolpiczak/ESC-50"
  done > "$TMP/recordings.tsv" || fail "couldn't list ESC-50's sounds"

# nasa <video|audio> <id> <derivative>: a manifest row for a file in NASA's image and video library.
nasa() {
  local kind="$1" id="$2" derivative="$3" encoded url title
  encoded="$(jq -rn --arg id "$id" '$id | @uri')"
  url="$(get "https://images-assets.nasa.gov/$kind/$encoded/collection.json" \
    | jq -r --arg d "~$derivative" '.[] | select(endswith($d))' | head -1 | sed 's/ /%20/g; s/^http:/https:/')" \
    || fail "couldn't list NASA's files for $id"
  title="$(get "https://images-api.nasa.gov/search?nasa_id=$encoded" \
    | jq -r '.collection.items[0].data[0].title // ""')" || fail "couldn't look up NASA's $id"
  [[ -n "$url" ]] || fail "NASA has no $derivative for $id"
  [[ "$kind" == audio ]] && url="remux:$url"
  row "$url" "${derivative##*.}" "NASA Image and Video Library" "$title" "NASA" "Public domain (NASA)" \
    "https://images.nasa.gov/details/$encoded"
}
for id in Ep391_Mars_Is_Hard Ep419_Telling_Time_On_Other_Worlds Ep439_Recordings_from_the_Far_Side_of_the_Moon; do
  nasa audio "$id" 64k.m4a
done >> "$TMP/recordings.tsv"
place "$TMP/recordings.tsv" Recordings "New Recording %d" 1

# --- Music: Musopen recordings on Wikimedia Commons, as MP3 -----------------------------------------------------------

echo "Music"
music=(
  "Chopin Nocturne No. 2 in E Flat Major, Op. 9.ogg"
  "Bizet - Carmen - Toreador Song (French, Musopen).ogg"
  "A Midsummer Night's Dream Op. 61 Song with choir (Mendelssohn) European Archive.ogg"
  "Antonio Vivaldi, Mandolin Concerto in C major, RV 425.ogg"
  "Beethoven - String Quartet No. 6 in B flat major, Op. 18 No. 6 - II. Adagio, ma non troppo (Musopen String Quartet).flac"
  "Antonin Dvorak - symphony no. 9 in e minor 'from the new world', op. 95 - iv. allegro con fuoco.ogg"
)
for file in "${music[@]}"; do
  get -G "https://commons.wikimedia.org/w/api.php" --data-urlencode "titles=File:$file" \
    -d "action=query&format=json&prop=videoinfo&viprop=url|derivatives|extmetadata" \
    -d "viextmetadatafilter=Artist|LicenseShortName" \
    | jq -r --arg title "${file%.*}" '.query.pages[] | .videoinfo[0] as $v
        | [([($v.derivatives // [])[] | select(.type | test("audio/mpeg")) | .src][0]), "mp3", "Musopen, on Wikimedia Commons",
           $title, ($v.extmetadata.Artist.value // "" | gsub("<[^>]*>"; "") | gsub("\\s+"; " ")),
           ($v.extmetadata.LicenseShortName.value // ""), $v.descriptionurl]
        | select(.[0] != null) | map(if . == "" then "–" else gsub("\t"; " ") end) | @tsv' \
    > "$TMP/track.tsv" && [[ -s "$TMP/track.tsv" ]] || fail "couldn't find an MP3 of $file"
  cat "$TMP/track.tsv"
done > "$TMP/music.tsv"
place "$TMP/music.tsv" Music "Track %02d" 1

# --- Videos: NASA, and home movies from the Prelinger Archives (their first 3 minutes) --------------------------------

echo "Videos"
for id in "KSC ART II Cause Way Launch low angle" "GSFC_20171004_m12738_RapidIntensification" \
  "JPL-20201221-M2020f-0002-EDL Full Version w SFX" "GSFC_20180314_Aurora_m12865_Steve" \
  "SS_SpaceFoodScientist_Final_Social" "TIME WARP TOTAL SOLAR ECLIPSE TIMELAPSE_HD" \
  "NHQ_2019_1018_all woman spacewalk"; do
  nasa video "$id" mobile.mp4
done > "$TMP/videos.tsv"
for id in HMChildren98343 HMChristmas97328 HMCanadianRockies97417; do
  get "https://archive.org/metadata/$id" | jq -r --arg id "$id" '
      [("trim:https://archive.org/download/" + $id + "/" + ([.files[] | select(.name | endswith(".mp4")) | .name][0])),
       "mp4", "Prelinger Archives (archive.org), first 3 minutes", (.metadata.title // $id),
       "Prelinger Archives", "Public domain", ("https://archive.org/details/" + $id)] | @tsv' \
    || fail "couldn't find $id on archive.org"
done >> "$TMP/videos.tsv"
place "$TMP/videos.tsv" Videos "VID_%04d" 3001

# --- Documents: Wikipedia articles as PDFs, in English and Dutch -----------------------------------------------------

echo "Documents"
pdfs="en:Lighthouse en:Western_honey_bee en:Bicycle en:Volcano en:Coffee en:Aurora en:Chess en:Penguin
en:Great_Barrier_Reef en:Electric_car nl:Deltawerken nl:Stroopwafel nl:Elfstedentocht nl:Windmolen nl:Tulp"
for page in $pdfs; do
  row "https://${page%%:*}.wikipedia.org/api/rest_v1/page/pdf/${page#*:}" pdf "Wikipedia" "${page#*:}" \
    "Wikipedia contributors" "CC BY-SA 4.0" "https://${page%%:*}.wikipedia.org/wiki/${page#*:}"
done > "$TMP/pdfs.tsv"
place "$TMP/pdfs.tsv" Documents "download (%d)" 1

# Notes: the start of a Wikipedia article, as Markdown, Word, a web page or RTF.
notes="en:Espresso nl:Hagelslag en:Marathon nl:Sinterklaas en:Jazz nl:Koningsdag en:Photosynthesis
nl:Waddenzee en:Tide nl:Tulpenmanie en:Houseplant nl:Erwtensoep en:Origami nl:Fietspad en:Sourdough nl:Goudse_kaas
en:Pizza nl:Afsluitdijk en:Rock_climbing nl:Oliebol"
formats=(md docx html rtf)
names=("Note %d" "Document %d" "Page %d" "Untitled %d")
n=0
for page in $notes; do
  i=$((n % 4))
  # shellcheck disable=SC2059
  file="$OUT/Documents/$(printf "${names[$i]}" $((n / 4 + 1))).${formats[$i]}"
  n=$((n + 1))
  lang="${page%%:*}" title="${page#*:}"
  if [[ ! -s "$file" ]]; then
    get -G "https://$lang.wikipedia.org/w/api.php" --data-urlencode "titles=$title" \
      -d "action=query&format=json&prop=extracts&explaintext=1&exsectionformat=plain&redirects=1" \
      | jq -r '.query.pages[] | .title + "\n" + ([(.extract // "") | split("\n")[] | select(length > 0)][:8] | join("\n"))' \
        > "$TMP/note.txt" || true
    if [[ $(grep -c . "$TMP/note.txt") -lt 2 ]]; then
      echo "  couldn't fetch $page" >&2
      continue
    fi
    heading="$(head -1 "$TMP/note.txt")"
    case "${formats[$i]}" in
      md) { printf '# %s\n\n' "$heading"; tail -n +2 "$TMP/note.txt" | awk '{ print; print "" }'; } > "$file" ;;
      *)
        {
          printf '<!doctype html><html><head><meta charset="utf-8"><title>%s</title></head><body>\n' "$heading"
          sed 's/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g' "$TMP/note.txt" | sed '1s/.*/<h1>&<\/h1>/; 2,$s/.*/<p>&<\/p>/'
          printf '</body></html>\n'
        } > "$TMP/note.html"
        if [[ "${formats[$i]}" == html ]]; then
          cp "$TMP/note.html" "$file"
        else
          textutil -convert "${formats[$i]}" -inputencoding UTF-8 "$TMP/note.html" -output "$file"
        fi
        ;;
    esac
  fi
  credit "Documents/${file##*/}" "Wikipedia" "${title//_/ } (the start)" "Wikipedia contributors" "CC BY-SA 4.0" \
    "https://$lang.wikipedia.org/wiki/$title"
done
echo "  Documents: $(find "$OUT/Documents" -type f | wc -l | tr -d ' ') files"

# --- Books: Project Gutenberg, as plain text -------------------------------------------------------------------------

# Two, not a shelf: a whole book has thousands of passages, and with many of them one fits nearly any query a little
# (try "piano music" with A Christmas Carol in the index).
echo "Books"
books=(
  "1661|The Adventures of Sherlock Holmes|Arthur Conan Doyle"
  "65061|The Boston Cooking-School Cook Book|Fannie Merritt Farmer"
)
for book in "${books[@]}"; do
  IFS='|' read -r id title author <<< "$book"
  row "https://www.gutenberg.org/cache/epub/$id/pg$id.txt" txt "Project Gutenberg" "$title" "$author" \
    "Public domain in the USA" "https://www.gutenberg.org/ebooks/$id"
done > "$TMP/books.tsv"
place "$TMP/books.tsv" Books "ebook %d" 1

mv "$CREDITS" "$OUT/CREDITS.csv"
echo
echo "$OUT: $(find "$OUT" -type f ! -name CREDITS.csv | wc -l | tr -d ' ') files, $(du -sh "$OUT" | cut -f1)"
echo "Add it to the index: sift-it folders add \"$OUT\""
