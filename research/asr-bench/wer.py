import sys, re, unicodedata
def norm(s):
    s = unicodedata.normalize("NFC", s.lower())
    s = re.sub(r"<[^>]+>", " ", s)          # lang tags etc.
    s = s.replace("’", "'").replace("'", " ")
    s = re.sub(r"[^\w\s]", " ", s)
    return s.split()
def ed(a, b):
    d = list(range(len(b)+1))
    for i in range(1, len(a)+1):
        p, d[0] = d[0], i
        for j in range(1, len(b)+1):
            p, d[j] = d[j], min(d[j]+1, d[j-1]+1, p + (a[i-1] != b[j-1]))
    return d[len(b)]
refs = {l.split("\t")[0]: l.rstrip("\n").split("\t")[2] for l in open(sys.argv[1])}
for path in sys.argv[2:]:
    errs = words = 0; ms = audio = 0.0; n = 0; worst = []; ne = nw = 0
    for line in open(path):
        parts = line.rstrip("\n").split("\t")
        if len(parts) < 3 or parts[0] not in refs: continue
        name, t, dur = parts[0], float(parts[1]), float(parts[2])
        hyp = parts[3] if len(parts) > 3 else ""
        r, h = norm(refs[name]), norm(hyp)
        e = ed(r, h); errs += e; words += len(r); ms += t; audio += dur; n += 1
        worst.append((e/len(r), name, hyp))
        if not re.search(r"\d", refs[name]): ne += e; nw += len(r)
    if n == 0 or nw == 0: continue
    print(f"{path}\tn={n}\tWER={100*errs/words:.2f}%\tmean_ms={ms/n:.0f}\tRTFx={audio*1000/ms:.1f}\tWER_nodigit={100*ne/nw:.2f}%")
