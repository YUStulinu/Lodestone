#!/usr/bin/env bash
# Downloads the sqllogictest files Lodestone is evaluated on, from a GitHub mirror of SQLite's
# sqllogictest (https://www.sqlite.org/sqllogictest, public domain), into data/slt/.
# The sample is fixed: all of select1-5 and evidence/, the first five files of each random/
# directory, and the 10-row variant (slt_good_0) of each index/ directory -- except view/, whose
# slt_good_0 is empty in the mirror, so slt_good_1 is used.
set -euo pipefail
cd "$(dirname "$0")/.."
BASE=https://raw.githubusercontent.com/gregrahn/sqllogictest/master/test
files=(select1 select2 select3 select4 select5)
for f in in1 in2 slt_lang_aggfunc slt_lang_createtrigger slt_lang_createview slt_lang_dropindex slt_lang_droptable \
         slt_lang_droptrigger slt_lang_dropview slt_lang_reindex slt_lang_replace slt_lang_update; do
  files+=("evidence/$f")
done
for d in aggregates expr groupby select; do
  for n in 0 1 2 3 4; do files+=("random/$d/slt_good_$n"); done
done
for d in between commute delete in orderby orderby_nosort random; do
  files+=("index/$d/10/slt_good_0")
done
files+=("index/view/10/slt_good_1")
for f in "${files[@]}"; do
  out="data/slt/$f.test"
  [[ -s $out ]] && continue
  mkdir -p "$(dirname "$out")"
  curl -fsSL --retry 3 -o "$out" "$BASE/$f.test"
done
echo "${#files[@]} files in data/slt"
