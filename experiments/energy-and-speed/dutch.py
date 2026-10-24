"""One normalization for Dutch transcripts, applied the same way to a reference and to what a model heard, so that
only words count as errors, not how they're written:

- NFKC, lower case, and no accents (één = een: the models write either);
- numbers as words, the way they're said: 1100-1999 as years ("1940" = negentienveertig), other whole numbers as
  cardinals (num2words), "11e" as an ordinal, "1,5" as één komma vijf, "10.000" as tienduizend, "%" as procent;
- hyphens, dashes, slashes and apostrophes split words ("M16-geweer" = m zestien geweer); other punctuation goes.

`merge_compounds` then joins two neighbouring words on one side when the other side has them as one word
(verkeers stroom / verkeersstroom), the way the Open ASR Leaderboard aligns compounds.
"""
import re
import unicodedata

from num2words import num2words


def _number(digits):
    n = int(digits)
    if len(digits) == 4 and 1100 <= n <= 1999:
        high, low = divmod(n, 100)
        return num2words(high, lang="nl") + ("honderd" if low == 0 else num2words(low, lang="nl"))
    return num2words(n, lang="nl")


def words(text):
    text = unicodedata.normalize("NFKC", text).lower()
    text = text.replace("%", " procent ")
    text = re.sub(r"(?<=\d)\.(?=\d{3}\b)", "", text)  # 10.000
    text = re.sub(r"(\d+),(\d+)", r"\1 komma \2", text)  # 1,5
    text = re.sub(r"\b(\d+)e\b", lambda m: num2words(int(m.group(1)), lang="nl", to="ordinal"), text)
    text = re.sub(r"\d+", lambda m: " " + _number(m.group(0)) + " ", text)
    text = re.sub(r"[-‐‑‒–—/'’`]", " ", text)
    text = unicodedata.normalize("NFKD", text)
    text = "".join(c for c in text if not unicodedata.combining(c))
    text = re.sub(r"[^\w\s]|_", " ", text)
    return text.split()


def _merge(side, other):
    vocabulary = set(other)
    out, i = [], 0
    while i < len(side):
        if i + 1 < len(side) and side[i] + side[i + 1] in vocabulary and side[i] + side[i + 1] not in side:
            out.append(side[i] + side[i + 1])
            i += 2
        else:
            out.append(side[i])
            i += 1
    return out


def merge_compounds(a, b):
    return _merge(a, b), _merge(b, a)


FILLERS = {"uh", "uhm", "um", "eh", "ehm", "euh", "hm", "hmm", "mm", "ah", "oh", "nou", "ja"}
