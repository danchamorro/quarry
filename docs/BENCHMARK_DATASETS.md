# Official public benchmark datasets

Quarry uses these two downloadable CSV files as its official public benchmark
datasets. They let anyone repeat the measurements with the same input files.
Use the extracted CSV files, with their original headers and contents.
ClickHouse is not required to run Quarry's benchmarks.

| Dataset | Source documentation | CSV filename |
|---|---|---|
| Taiwan historical weather, preprocessed 1896–2023 | [ClickHouse: Taiwan weather](https://clickhouse.com/docs/get-started/sample-datasets/tw-weather) | `daily_weather_preprocessed_1896_2023.csv` |
| Hacker News | [ClickHouse: Hacker News](https://clickhouse.com/docs/get-started/sample-datasets/hacker-news) | `hacknernews.csv` |

The Hacker News filename is spelled `hacknernews` upstream. Keep that spelling
when following the commands below. For weather, use the preprocessed archive,
not the separate raw-data archive.

## Download and extract

These commands work on macOS and Linux. Run them from a location with room for
both the compressed downloads and extracted files. They create a new directory
and stop if it already exists, so existing benchmark inputs are not replaced.

```bash
set -eu
mkdir quarry-benchmark-data
cd quarry-benchmark-data

curl --fail --location \
  --output preprocessed_weather_daily_1896_2023.tar.gz \
  https://storage.googleapis.com/taiwan-weather-observaiton-datasets/preprocessed_weather_daily_1896_2023.tar.gz
tar -xzf preprocessed_weather_daily_1896_2023.tar.gz \
  daily_weather_preprocessed_1896_2023.csv

curl --fail --location \
  --output hacknernews.csv.gz \
  https://datasets-documentation.s3.eu-west-3.amazonaws.com/hackernews/hacknernews.csv.gz
gzip -dc hacknernews.csv.gz > hacknernews.csv
```

The download locations above come from the linked ClickHouse documentation,
checked on 2026-09-11. Download and decompression time are not part of Quarry's
benchmark timings.

## Check the input files

ClickHouse publishes these MD5 checksums for the weather files:

| File | Upstream MD5 |
|---|---|
| `preprocessed_weather_daily_1896_2023.tar.gz` | `11b484f5bd9ddafec5cfb131eb2dd008` |
| `daily_weather_preprocessed_1896_2023.csv` | `1132248c78195c43d93f843753881754` |

On macOS, run:

```bash
md5 preprocessed_weather_daily_1896_2023.tar.gz daily_weather_preprocessed_1896_2023.csv
shasum -a 256 daily_weather_preprocessed_1896_2023.csv hacknernews.csv
```

On Linux, run:

```bash
md5sum preprocessed_weather_daily_1896_2023.tar.gz daily_weather_preprocessed_1896_2023.csv
sha256sum daily_weather_preprocessed_1896_2023.csv hacknernews.csv
```

Compare the weather MD5 values with the table above. The Hacker News source
page does not publish a checksum. Compare both extracted files' SHA-256 values
with the measured input identities in the
[public-dataset benchmark report](benchmarks/2026-09-11-public-datasets.md).
Matching hashes establish that a repeat run used the same bytes as the report.

## Repeat the benchmarks

From a clean Quarry checkout, run the sequential suite against the directory
containing both extracted CSVs. Commit or set aside tracked and untracked
changes first. The runner builds the release CLI with locked dependencies and
verifies that the checkout stays clean at the same commit before recording it:

```bash
python3 scripts/benchmark-public-datasets.py \
  /absolute/path/quarry-benchmark-data \
  /absolute/path/new-quarry-benchmark-results
```

The results directory must not already exist. Build time is outside the recorded
benchmark interval. The script writes separate log files and records their
filenames, exact commands, source hashes, and timings in `results.json`.
Successful generated outputs are removed after the CLI checks finish; failed
cases retain their output
directory for inspection. The input CSVs are preserved. Allow room for sort
working files and run without competing disk-heavy jobs.

The [public-dataset benchmark report](benchmarks/2026-09-11-public-datasets.md)
records the input sizes and hashes, Quarry revision, machine, cache conditions,
validation, results, and measured temporary disk use. Use the same revision and
verify matching input hashes when comparing a repeat run.

Earlier reports using `LARGE_FILE_12GB.csv` and `LARGE_FILE_50GB.csv` describe
different local fixtures. Their results remain historical measurements of
those inputs; they are not measurements of these public datasets.
