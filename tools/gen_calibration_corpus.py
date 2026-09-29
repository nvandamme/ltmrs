"""Deterministic retrieval-calibration corpus generator (seeded, no network).

Design (WP-12 T-QUALITY calibration):
- 10 topics x 30 memories = 300 memories with distinctive vocabularies.
- 1 query per memory, phrased as a question using SYNONYMS of the
  memory's distinctive terms (paraphrase, not keyword overlap), so the
  task measures semantic ranking rather than lexical matching.
- Topic-disjoint splits: topics[0:7] -> dev (210), topics[7:10] ->
  heldout (90). No query or target crosses splits.
- Overlap audit printed at the end (shared content tokens query/target).

Output: experiments/quality/retrieval-calibration.json
  {memories: [{key,title,fragment,topic}], cases: [{query,targets,split}]}
"""
import json
import random
import re
import sys

RNG = random.Random(20260928)
STOP = set(
    "the a an of and or to in on for with is are was were be by as at "
    "from that this it its into over under what which who how why when "
    "does do did can could should would there their them they he she we "
    "you your my our his her its about than then than so such no nor not "
    "only own same too very will just don should now".split()
)

# topic: (terms[12], synonym-pairs[(src->dst)], memory-frames, question-frames)
TOPICS = {
    "sourdough": (
        ["levain", "hydration", "autolyse", "banneton", "crumb", "scoring",
         "bulk-ferment", "discard", "dutch-oven", "starter", "proofing", "ear"],
        [("levain", "fermented culture"), ("hydration", "water ratio"),
         ("banneton", "proofing basket"), ("crumb", "interior texture"),
         ("scoring", "slashing the top"), ("dutch-oven", "covered pot"),
         ("starter", "mother culture"), ("proofing", "final rise")],
        ["{t1} depends on {t2}: without proper {t2} the {t3} never develops.",
         "Bakers adjust {t1} when the {t2} looks off, then check {t3} before baking.",
         "A strong {t1} plus patient {t2} gives an open {t3} every time.",
         "Cold retard shapes {t1}; morning {t2} decides the {t3}."],
        ["How does {s1} change the final loaf?",
         "What goes wrong when {s1} is neglected?"],
    ),
    "kubernetes": (
        ["pod", "deployment", "ingress", "helm-chart", "kubelet", "namespace",
         "sidecar", "operator", "statefulset", "kubectl", "service-mesh", "taint"],
        [("pod", "smallest deployable unit"), ("ingress", "inbound traffic rules"),
         ("helm-chart", "packaged release bundle"), ("kubelet", "node agent"),
         ("sidecar", "companion container"), ("kubectl", "command-line client"),
         ("namespace", "isolation scope"), ("operator", "automated controller")],
        ["A {t1} restart often traces back to a misconfigured {t2} in the same {t3}.",
         "Operators tune {t1} limits after watching {t2} saturate across the {t3}.",
         "The {t1} stayed pending until the {t2} quota inside {t3} was raised.",
         "Debugging {t1} starts with {t2} logs, then the events on {t3}."],
        ["Why would {s1} keep restarting?",
         "How do you diagnose a stuck {s1}?"],
    ),
    "jazz": (
        ["voicing", "comping", "turnaround", "blue-note", "syncopation",
         "walking-bass", "head-solo-head", "trading-fours", "reharmonization",
         "modal-interchange", "vamp", "break"],
        [("voicing", "chord layout"), ("comping", "rhythmic accompaniment"),
         ("blue-note", "flattened expressive pitch"), ("syncopation", "off-beat accent"),
         ("walking-bass", "stepwise bass line"), ("vamp", "repeated groove figure"),
         ("turnaround", "closing progression"), ("trading-fours", "four-bar exchanges")],
        ["Her {t1} under the soloist left room for a daring {t2} before the {t3}.",
         "The band stretched {t1} across twelve bars of {t2} into the {t3}.",
         "Good {t1} means listening first: answer the {t2}, then set up the {t3}.",
         "They rehearsed {t1} slowly until the {t2} locked with the {t3}."],
        ["What makes {s1} sit well under a solo?",
         "How should a rhythm section handle {s1}?"],
    ),
    "beekeeping": (
        ["apiary", "super", "brood-box", "queen-excluder", "smoker",
         "varroa", "swarm", "honey-flow", "propolis", "nuc", "uncapping", "extractor"],
        [("super", "honey storage box"), ("queen-excluder", "worker-only grid"),
         ("varroa", "parasitic mite"), ("swarm", "departing colony cluster"),
         ("propolis", "resinous sealant"), ("smoker", "calming smoke tool"),
         ("brood-box", "nursery chamber"), ("honey-flow", "nectar season")],
        ["Check {t1} before adding another {t2} ahead of the {t3}.",
         "A failing {t1} explains spotty brood beside a healthy {t2} during {t3}.",
         "Veteran keepers read {t1} the way sailors read clouds before {t2} in a {t3}.",
         "Treat {t1} early, or {t2} collapses right at the peak of {t3}."],
        ["When should you add another {s1}?",
         "What does a failing {s1} look like?"],
    ),
    "roman-concrete": (
        ["pozzolana", "opus-caementicium", "aggregate", "curing", "arch",
         "vault", "pantheon", "lime", "formwork", "coffer", "abutment", "keystone"],
        [("pozzolana", "volcanic ash additive"), ("aggregate", "stone filler mix"),
         ("curing", "slow hardening"), ("vault", "arched ceiling span"),
         ("formwork", "wooden molding"), ("abutment", "supporting mass"),
         ("lime", "binder base"), ("arch", "curved load path")],
        ["Roman {t1} outlasts modern pours because {t2} keeps reacting inside the {t3}.",
         "The {t1} at the site still shows {t2} impressions around every {t3}.",
         "Builders chose {t1} where {t2} met seawater, trusting the {t3} to hold.",
         "Without proper {t1}, even thick {t2} cracks across the {t3} within decades."],
        ["Why does Roman {s1} outlast modern pours?",
         "What role does {s1} play at the building site?"],
    ),
    "transformers": (
        ["attention-head", "key-query-value", "positional-encoding",
         "layer-norm", "softmax", "token", "context-window", "temperature",
         "kv-cache", "perplexity", "logit", "embedding-table"],
        [("attention-head", "parallel focus unit"), ("softmax", "normalized weighting"),
         ("token", "text piece"), ("context-window", "input span limit"),
         ("perplexity", "surprise metric"), ("logit", "raw output score"),
         ("layer-norm", "per-layer rescaling"), ("temperature", "randomness dial")],
        ["Ablating one {t1} barely moves loss when {t2} is saturated across the {t3}.",
         "The {t1} gradient vanishes unless {t2} is stabilized before the {t3}.",
         "Engineers profile {t1} first when {t2} latency dominates the {t3}.",
         "Tuning {t1} trades {t2} sharpness against stability of the {t3}."],
        ["What happens when you ablate a single {s1}?",
         "How does {s1} affect inference cost?"],
    ),
    "napoleonic-logistics": (
        ["corps-system", "foraging", "supply-depot", "artillery-train",
         "bivouac", "pontoon", "grande-armee", "caisson", "defile",
         "cantonment", "requisition", "march-table"],
        [("corps-system", "self-contained army units"), ("foraging", "living off the land"),
         ("supply-depot", "forward stockpile"), ("bivouac", "open-air camp"),
         ("pontoon", "floating bridge span"), ("caisson", "ammunition wagon"),
         ("defile", "narrow pass"), ("cantonment", "winter quarters")],
        ["The {t1} collapsed once {t2} failed ahead of the advancing {t3}.",
         "Staff officers planned {t1} around {t2} capacity, not wishes, before each {t3}.",
         "Rain turned {t1} to mud, stranding {t2} short of the {t3}.",
         "Victory depended less on guns than on {t1} feeding {t2} toward the {t3}."],
        ["Why did the {s1} collapse in that campaign?",
         "What mattered more than guns for reaching the {s1}?"],
    ),
    "quantum-spin": (
        ["qubit", "superposition", "entanglement", "decoherence",
         "bell-state", "measurement", "bloch-sphere", "fidelity",
         "hamiltonian", "eigenbasis", "tunneling", "annealing"],
        [("qubit", "two-level system"), ("superposition", "joint possibilities"),
         ("decoherence", "environmental leakage"), ("bell-state", "maximally linked pair"),
         ("fidelity", "overlap score"), ("measurement", "readout collapse"),
         ("bloch-sphere", "state globe"), ("entanglement", "nonlocal correlation")],
        ["Protecting {t1} means shielding {t2} until just before {t3}.",
         "The experiment varied {t1} while tracking {t2} across every {t3}.",
         "Theory predicts {t1} survives only if {t2} stays below the {t3} floor.",
         "They inferred {t1} indirectly, since direct {t2} destroys the {t3}."],
        ["How do you protect {s1} in practice?",
         "What destroys {s1} fastest?"],
    ),
    "medieval-sieges": (
        ["trebuchet", "portcullis", "battlement", "siege-tower", "moat",
         "sally-port", "mangonel", "keep", "barbican", "murder-hole",
         "counterweight", "escarpment"],
        [("trebuchet", "long-arm thrower"), ("portcullis", "toothed iron gate"),
         ("battlement", "crenellated parapet"), ("siege-tower", "rolling assault frame"),
         ("sally-port", "secret sortie door"), ("mangonel", "torsion thrower"),
         ("barbican", "outer gatehouse"), ("moat", "water ditch")],
        ["The {t1} held for weeks because {t2} covered every approach to the {t3}.",
         "Sappers tunneled past {t1} while {t2} drew fire away from the {t3}.",
         "A single night sortie through {t1} burned the {t2} before the {t3} fell.",
         "Commanders starved {t1} rather than storm {t2} above the {t3}."],
        ["Why did the {s1} hold for weeks?",
         "How do sappers get past a {s1}?"],
    ),
    "telescope-optics": (
        ["aperture", "focal-ratio", "coma", "chromatic-aberration",
         "eyepiece", "mount", "refractor", "reflector", "collimation",
         "seeing", "dew-shield", "finder"],
        [("aperture", "light-gathering width"), ("focal-ratio", "speed number"),
         ("coma", "edge smear"), ("eyepiece", "magnifying lens set"),
         ("collimation", "mirror alignment"), ("seeing", "air steadiness"),
         ("refractor", "lens tube"), ("mount", "tracking base")],
        ["Doubling {t1} beats any upgrade to {t2} under typical {t3}.",
         "The {t1} showed {t2} at the field edge until {t3} improved.",
         "Beginners blame {t1} when the real culprit is poor {t2} during bad {t3}.",
         "Test {t1} on a bright star before trusting {t2} for faint {t3}."],
        ["Why upgrade {s1} before anything else?",
         "What do beginners blame instead of poor {s1}?"],
    ),
}

def toks(s):
    return [t for t in re.findall(r"[a-z0-9]+(?:-[a-z0-9]+)?", s.lower())
            if t not in STOP and len(t) > 3]


def main():
    memories, cases = [], []
    overlap_counts = []
    for ti, (topic, (terms, syns, frames, qframes)) in enumerate(TOPICS.items()):
        split = "dev" if ti < 7 else "heldout"
        syn = dict(syns)
        for mi in range(30):
            t = [terms[(mi * 3 + k) % len(terms)] for k in range(3)]
            frame = frames[(mi + ti) % len(frames)]
            frag = frame.format(t1=t[0], t2=t[1], t3=t[2])
            title = f"{t[0]} {t[1]} notes".replace("-", " ")
            key = f"t{ti:02d}-m{mi:02d}"
            memories.append({"key": key, "title": title,
                             "fragment": frag, "topic": topic})
            # query: substitute the first distinctive term with its synonym
            qterms = [syn.get(t[k], t[k]) for k in range(3)]
            qf = qframes[(mi + ti) % len(qframes)]
            # fill s1 with the first substituted span (or raw term)
            s1 = qterms[0]
            query = qf.format(s1=s1)
            cases.append({"query": query, "targets": [key], "split": split})
            overlap_counts.append(
                len(set(toks(query)) & set(toks(title + " " + frag))))
    out = {"memories": memories, "cases": cases,
           "meta": {"topics": list(TOPICS), "dev_topics": 7,
                    "seed": 20260928,
                    "note": "synthetic known-answer corpus; queries paraphrase via per-topic synonyms"}}
    with open("experiments/quality/retrieval-calibration.json", "w") as f:
        json.dump(out, f, indent=1)
        f.write("\n")
    import statistics
    print(f"memories={len(memories)} cases={len(cases)} "
          f"dev={sum(1 for c in cases if c['split']=='dev')} "
          f"heldout={sum(1 for c in cases if c['split']=='heldout')}")
    print(f"query-target shared tokens: mean={statistics.mean(overlap_counts):.2f} "
          f"max={max(overlap_counts)}")


if __name__ == "__main__":
    main()
