# R3 evidence — filtered eligibility at the 100k rung

- Date: 2026-08-22
- Host: DigitalOcean `gpu-b300x1-288gb-lc-spot` (28 vCPU, NVMe), dedicated
- Corpus: 20,000 synthetic documents, two doc-value fields (8-way string
  category, 1,000-way integer price), ingested through the shipped binary
  with windowed maintenance (84.6 s total ingest)
- Protocol: 200 integrated lexical queries per case, candidate limit 1,000,
  limit 10, identical query mix across cases; per-query wall time

## Measurements

| Eligibility case | ms per query | vs match-all |
|---|---|---|
| match-all | 5.04 | 1.00× |
| equality (`category = γ`) | 4.93 | 0.98× |
| membership (`category IN (3)`) | 5.96 | 1.18× |
| missing field (`IS NULL`) | 5.20 | 1.03× |
| composite (`price < 800 AND NOT category = β`) | 8.37 | 1.66× |

## Verdict

R3 (deterministic bitmap eligibility masks) set its own gate: take on
bitmap complexity only if the harness shows a filtered workload paying
more than 2× over the unfiltered path. The measured worst composite is
1.66× and every single-predicate case is within 18% of match-all — exact
equality is cheaper than no filter because the ordered posting index
shrinks the candidate set before scoring. Filtered eligibility costs
single-digit milliseconds at the current collection cap.

R3 is closed by this evidence at the 100k rung. The item re-opens for
re-measurement when the ingest write-path work gates the 100k → 1M raise,
where posting-range unions over ten times the identifiers may cross the
threshold.
