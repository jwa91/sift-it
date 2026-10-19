# Ranking on a mixed folder

The testbed (`make testbed`) is small and every file in it is short, so it can't show what long files and ordinary
words do to the ranking. A demo folder can: 522 files from public collections, eight books among photos, sounds,
recordings, music, videos, PDFs and notes (6,500 vectors). Two problems showed up there, both fixed in the ranking
(`src/query/rank.rs`).

## Reproducing

The demo folder isn't part of the repository: its files come from public collections, each under its own license. The
script that fetches them is in the repository's history, and six Project Gutenberg books go beside it:

```sh
git show 647b4f3:scripts/make-demo.sh > /tmp/make-demo.sh
bash /tmp/make-demo.sh /tmp/demo                # the demo, with two books
mkdir -p /tmp/demo-books                        # six more, in a folder of their own
for id in 11 21 46 84 132 11024; do
  curl -fsSL -o /tmp/demo-books/"ebook $id.txt" "https://www.gutenberg.org/cache/epub/$id/pg$id.txt"
done
export SIFT_IT_HOME=/tmp/demo-home              # an index of its own
sift-it folders add /tmp/demo /tmp/demo-books
sift-it config sources.ignore '["CREDITS.csv"]' # it describes every file, so it would answer the queries itself
sift-it index                                   # about an hour, most of it writing down what the podcasts say
sift-it eval docs/demo-queries.json
```

`docs/demo-queries.json` has the queries with the files that answer them, by name: 107 with an answer and 5 without.
It isn't part of `make eval`, because it needs the demo and the books. `ebook N.txt` is Gutenberg book N (11 Alice,
21 Aesop's Fables, 46 A Christmas Carol, 84 Frankenstein, 132 The Art of War, 11024 Max Havelaar; in the demo,
`ebook 1.txt` is the Boston Cooking-School Cook Book and `ebook 2.txt` Sherlock Holmes). The demo's other names
(`IMG_1210.jpg`, `New Recording 12.wav`) are as `make-demo.sh` placed them on 2026-10-14; `CREDITS.csv` says what each
one is, should a later run place them differently.

## Long files

A file counts by its best segment, and the best of hundreds of segments fits nearly any query a little. A book has a
passage about church bells, a crying baby, a lightning storm: often a real one (Frankenstein's oak struck by
lightning), but not what someone typing "church bells" is after. Before the change, a book came first for 21 of the 95
demo queries whose answer isn't a book.

The fix lowers every lift in a file of n segments by 0.0125 × ln(n / 4). A file of up to four segments loses nothing,
a 3-minute video 0.03, a book 0.05–0.07. That's about how far the best of n segments stands above a typical one by
chance alone: the expected maximum of n draws at the usual spread of a query's scores (0.03), less that of four. It
applies to everything ranked: meaning matches, word matches, and the bars for shown (0.09) and strong (0.13).

Tried and dropped: the same correction with each query's own spread (about the same results, with more parts); with each
file's own spread (worse: a video's scenes spread as much as a book's passages); offsetting the penalty by how much
the whole file lifts, which is high for a podcast about the query (worse overall). Up to four segments are free
because short files have few chances, and because on the testbed a 4-segment podcast leads a note by 0.006.

## Ordinary words

A segment with all of the query's words got a bonus of 0.10, as much as the gap between a weak and a strong match. That
bonus is for what no vector holds exactly: invoice numbers, codes. For ordinary words it double-counts, since the model
has already read them, and a passage that merely mentions them overtook better non-text matches. The espresso note
(it says "steam" and "train" about the word's origin) came above the photo of a steam train, the Lighthouse article
above the photo of lightning, and the Jazz note above the music.

Now the full bonus goes only to a query the words decide: one with a number in it, a `code:` search, or words in
quotes. Other words get 0.03, enough to choose between matches the meaning scores about the same. A match found by
ordinary words is strong if it scores at least 0.13, or if nothing found by meaning alone scores higher. That keeps a
name strong: "Bram" lifts only 0.03 in the meeting note that mentions him, but nothing else matches him. It stops a
cookbook passage holding "glass" and "breaking" (score 0.02) from being listed above the recording of breaking glass,
which is what a strong word match used to do.

Tried and dropped: a bonus that grows with the segment's lift (a fable about an eagle still came above the photo of
an eagle); a bonus only where the lift is meaningful (numbers lift about 0 in the page that has them, and unrelated
pages lift 0.13–0.17 for a number, so invoice numbers would no longer be found); strong only by score (every person's
name would show as a weaker match). How rare the words are doesn't tell names from ordinary words either: in the demo
"kat" and "puppies" are in one segment each, "Sherlock" in 88.

## Results

| | before | after |
|---|---|---|
| Testbed (`make eval`) | 43/43 top 1, 9/9 places, 0/4 false alarms | the same |
| Demo queries (`docs/demo-queries.json`) | 70/107 top 1, MRR 0.750, 1/5 false alarms | 82/107, MRR 0.829, 0/5 |
| A book first, where the answer isn't a book | 21 of 95 | 1 of 95 |
| Queries for a book, with that book first | 12 of 12 | 12 of 12 |
| 138 names, numbers and rare words, each for the demo file it came from | 123 first | 129 first |
| The testbed's queries in one index with the demo and the books | 34/43 top 1, 2/4 false alarms | 39/43, 1/4 |

The eight queries that showed the problems (top three; ᵂ: a match by words):

| query | before | after |
|---|---|---|
| piano music | A Christmas Carolᵂ, Jazz noteᵂ, Dvořák symphony | church bells clip, inside of a grand piano, **Chopin nocturne** (0.148, 0.148, 0.147) |
| rain | Sherlock Holmesᵂ, Frankensteinᵂ, Volcano PDFᵂ | sea waves clip, hurricane video, lightning photo (all weak) |
| church bells | A Christmas Carolᵂ, Dvořák symphony, **church bells clip** | **church bells clip**, Dvořák symphony, a bell tower on Santorini |
| a crying baby | **crying baby clip**, Alice, rooster clip | **crying baby clip**, rooster clip, dog clip |
| christmas presents | A Christmas Carolᵂ, Aliceᵂ, **Christmas home movie** | A Christmas Carolᵂ, Aliceᵂ, a room decorated for Christmas; **the home movie** 4th |
| een kat | Max Havelaarᵂ, Aesop's Fables, Alice | car horn clip, **calico cat**, frog clip, **odd-eyed cat** (all weak) |
| a steam train | espresso noteᵂ, **steam train in a home movie**, **steam locomotive** | **steam locomotive**, **the home movie**, vacuum cleaner clip; **steam train in snow** 4th |
| lightning storm | Frankensteinᵂ, Lighthouse PDFᵂ, **lightning photo** | **lightning photo**, **lightning over a tree**, **lighthouse in a lightning storm** |

Not fixed, because the model can't tell: the ESC-50 rain clip doesn't score near "rain" at all; for the Dutch
"een kat", cat photos lift only 0.10; "piano music" is a three-way tie within 0.001; and some sounds are mistaken for
others (a rooster for "an ambulance siren", crows for "a rooster crowing"). In "christmas presents", A Christmas Carol
and Alice have real passages about Christmas presents, and the home movie's 45 frames cost it 0.03.
