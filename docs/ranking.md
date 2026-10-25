# Ranking

sift-it ranks every kind of file in one list: documents, pictures, sound and what's said, with scores on one scale.
`src/query/rank.rs` does it, with the constants in `config.json`'s `ranking` section.

## From a query to results

1. **Candidates.** Segments come from two indexes: the vectors, by meaning, and the full-text index, by words. A kind
   filter applies first, before any limit. A quoted phrase keeps only the segments that have it, in order.
2. **Lift.** A segment's meaning counts by how far its cosine stands above the query's median. A vague query lands
   close to everything, and the lift takes that out. EmbeddingGemma 2 puts text, pictures and sound on one scale, so
   one median serves them all. It comes from a fixed sample of 8,192 vectors, and in a small index it's pulled toward
   a typical median, 0.55, as if the index held 30 more segments scoring that: with only a file or two, the median
   would be the match itself.
3. **Length.** A file counts by its best segment, and the best of hundreds fits nearly any query a little: a book has
   a passage about church bells, a crying baby, and Christmas presents. So every lift in a file of n segments is
   lowered by 0.0125 × ln(n / 4). A file of up to four segments (a photo, a few pages, a minute and a half of sound)
   loses nothing, a 3-minute video 0.03, a book 0.05 to 0.07. That's about how far the best of n segments stands above
   a typical one by chance alone: the expected maximum of n draws at the usual spread of a query's scores (0.03), less
   that of four.
4. **Room for every file.** At most 10 segments of one file are candidates, so a long file can't use up the places.
5. **Words.** A segment with all of the query's words (the last one as a prefix, while it's typed) gets a bonus. When
   the words decide, because the query has a number in it, searches code (`code:`), or quotes its words, the bonus is
   0.10: no vector holds an invoice number exactly. Otherwise it's 0.03. The model has already read ordinary words, so
   they only choose between matches whose meaning scores about the same, and a note that mentions steam and a train
   doesn't come before the photo of a steam train.
6. **Shown and strong.** A match by meaning is shown from a lift of 0.09, and strong from 0.13. On the testbed, answers
   lift 0.12 to 0.30, and the best results for queries without an answer 0.05 to 0.10, which makes "nothing found" an
   honest answer. A match by words is strong when the words decide, when it scores 0.13, or when nothing found by
   meaning alone scores higher. A name mentioned in one meeting note stays strong, and a cookbook passage that holds
   "glass" and "breaking" doesn't come before the audio clip of breaking glass.
7. **Results.** Strong matches first, then the weaker ones, each by score. A result opens at its best segment, and its
   segments within 0.04 of that are kept as other places to open.

| `ranking` key | Default | |
|---|---:|---|
| `shown_lift` | 0.09 | the lift a match by meaning needs to be shown |
| `strong_lift` | 0.13 | the lift, or the score by words, that makes a match strong |
| `word_bonus` | 0.10 | the bonus for all of the query's words, when they decide |
| `plain_word_bonus` | 0.03 | the bonus for all of them otherwise |
| `length_penalty`, `length_free` | 0.0125, 4 | a file of n segments loses `length_penalty` × ln(n / `length_free`) |
| `close` | 0.04 | how close to its best a file's other places must score |
| `per_file` | 10 | the candidates one file may have |
| `prior_median`, `prior_weight` | 0.55, 30 | the median a small index is pulled toward, and how hard |

## How well

`make eval` builds the testbed (`scripts/make-testbed.sh`: notes, documents, PDFs, photos, screenshots, records,
audio, a video and code, with names that say nothing about what's in them), indexes it, and runs the 59 queries
in `evals/evals.json`. With sift-it 0.4.1:

| | |
|---|---|
| The right file first | 54 of 55 |
| At the right page or moment | 19 of 20 |
| False alarms, for queries without an answer | 0 of 4 |

The two misses: "someone who can fix a leaking tap" finds `contacts.csv` first, but too weakly to show it, since a
table's many rows lower its score; and "penguin in the film" opens the video where the narration names the penguin, a
few seconds before it's shown.

A larger collection shows what long files and ordinary words do to the ranking, which the testbed's short files can't:
some 520 files from public collections, with photos, sound clips, voice memos, podcasts, music, videos, PDFs, notes, and
eight books. `docs/demo-queries.json` has 109 queries with the files that answer them, and 5 without an answer.

```sh
git show 647b4f3:scripts/make-demo.sh > /tmp/make-demo.sh
bash /tmp/make-demo.sh /tmp/demo                # the collection, with two books
mkdir -p /tmp/demo-books                        # six more, in a folder of their own
for id in 11 21 46 84 132 11024; do
  curl -fsSL -o /tmp/demo-books/"ebook $id.txt" "https://www.gutenberg.org/cache/epub/$id/pg$id.txt"
done
export SIFT_IT_HOME=/tmp/demo-home              # an index of its own
sift-it folders add /tmp/demo /tmp/demo-books
sift-it config sources.ignore '["CREDITS.csv"]' # it describes every file, so it would answer the queries itself
sift-it index
sift-it eval docs/demo-queries.json
```

The files come from public collections, each under its own license, so they aren't in the repository. `ebook N.txt`
is Project Gutenberg's book N: 11 Alice, 21 Aesop's Fables, 46 A Christmas Carol, 84 Frankenstein, 132 The Art of
War, 11024 Max Havelaar. In the collection, `ebook 1.txt` is the Boston Cooking-School Cook Book and `ebook 2.txt`
Sherlock Holmes. The other names (`IMG_1210.jpg`, `New Recording 12.wav`) are as `make-demo.sh` placed them on
2026-10-14; `CREDITS.csv` says what each file is, should a later run place them differently.

## Limits

What the model can't tell, ranking can't fix. On the demo collection:

- **Some sounds.** A rain clip doesn't score near "rain" at all, and some sounds are taken for others: a rooster for
  "an ambulance siren", crows for "a rooster crowing".
- **Other languages, a little.** For the Dutch "een kat", photos of cats lift only 0.10.
- **Long files with a real match.** A Christmas Carol and Alice have real passages about Christmas presents, so for
  "christmas presents" they come before the Christmas home movie, whose 45 frames cost it 0.03.
