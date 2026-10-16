import sys, tarfile, urllib.request, os, csv, io
lang, n, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
os.makedirs(out, exist_ok=True)
base = f"https://huggingface.co/datasets/google/fleurs/resolve/main/data/{lang}"
tsv = urllib.request.urlopen(f"{base}/test.tsv").read().decode()
refs = {}
for row in csv.reader(io.StringIO(tsv), delimiter="\t", quoting=csv.QUOTE_NONE):
    refs[row[1]] = (row[2], row[3])  # raw, normalized
resp = urllib.request.urlopen(f"{base}/audio/test.tar.gz")
got = 0
with tarfile.open(fileobj=resp, mode="r|gz") as tf, open(f"{out}/refs.tsv", "w") as rf:
    for m in tf:
        if not m.isfile(): continue
        name = os.path.basename(m.name)
        if name not in refs: continue
        data = tf.extractfile(m).read()
        open(f"{out}/{name}", "wb").write(data)
        rf.write(f"{name}\t{refs[name][0]}\t{refs[name][1]}\n")
        got += 1
        if got >= n: break
print(lang, got)
