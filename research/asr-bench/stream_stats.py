import sys, statistics as st
for p in sys.argv[1:]:
    rows=[l.rstrip('\n').split('\t') for l in open(p)]
    rows=[r for r in rows if len(r)>=7]
    fin=[float(r[1]) for r in rows]; mx=[float(r[5]) for r in rows]; first=[float(r[6]) for r in rows]
    rt=[float(r[4])/(float(r[2])*1000) for r in rows]
    q=lambda v,x: sorted(v)[int(x*(len(v)-1))]
    print(f"{p}\tn={len(rows)}\tfinalize p50={q(fin,.5):.0f} p90={q(fin,.9):.0f}ms\tmax_feed p90={q(mx,.9):.0f}ms\tcompute/audio={st.mean(rt):.3f}\tfirst_text={st.mean(first):.1f}s")
