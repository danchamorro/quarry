#!/usr/bin/env python3
"""Independent full-byte oracle for the 5M-row date-time-sort-fixture.py large.csv."""
import csv
import hashlib
import json
import sys

source, row_output, column_output = sys.argv[1:]
with open(source, "rb") as stream:
    source_bytes = stream.read(1024 * 1024)
    source_hash = hashlib.sha256()
    row_hash = hashlib.sha256()
    while source_bytes:
        source_hash.update(source_bytes)
        row_hash.update(source_bytes)
        source_bytes = stream.read(1024 * 1024)
row_hash.update(b",,\n" * 10)

class HashWriter:
    def __init__(self):
        self.hash = hashlib.sha256()
        self.bytes = 0
    def write(self, value):
        encoded = value.encode("utf-8")
        self.hash.update(encoded)
        self.bytes += len(encoded)
        return len(value)

sink = HashWriter()
writer = csv.writer(sink, lineterminator="\n")
records = 0
with open(source, encoding="utf-8", newline="") as stream:
    for record in csv.reader(stream):
        if len(record) != 3:
            raise ValueError("expected the three-column benchmark fixture")
        record[1:1] = ["New" if records == 0 else "", ""]
        writer.writerow(record)
        records += 1
assert records == 5_000_001, records

def actual_hash(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        while True:
            chunk = stream.read(1024 * 1024)
            if not chunk:
                return digest.hexdigest()
            digest.update(chunk)

row_expected, column_expected = row_hash.hexdigest(), sink.hash.hexdigest()
assert actual_hash(row_output) == row_expected, "row output differs"
assert actual_hash(column_output) == column_expected, "column output differs"
print(json.dumps(dict(source_sha256=source_hash.hexdigest(),
    row_sha256=row_expected, column_sha256=column_expected,
    source_records=records, row_records=records + 10, column_records=records,
    column_bytes=sink.bytes), indent=2))
