import sys
for p in sys.argv[1:]:
    ms = sorted(float(l.split("\t")[1]) for l in open(p))
    print(f"{p}\tn={len(ms)}\tp50={ms[len(ms)//2]:.0f}ms\tp90={ms[int(.9*(len(ms)-1))]:.0f}ms")
