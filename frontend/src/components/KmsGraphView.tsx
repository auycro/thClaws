import { useEffect, useMemo, useRef, useState } from "react";
import { X, Network } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import type { ViewerTarget } from "./KmsBrowserSidebar";

/// M6.39.13: Obsidian-style force-directed graph view of one KMS.
/// Nodes = pages, edges = `[[wikilinks]]`. Hand-rolled physics
/// (no d3 dep) — works comfortably up to ~100 nodes which covers
/// every realistic KMS. Click a node to open the page in the
/// viewer overlay; ESC or close button returns to the underlying
/// chat/terminal view.
///
/// Coordinates: each node has `x, y, vx, vy`. Per tick:
///   - repulsion: every node pushes every other (1/r²)
///   - attraction: each edge pulls its endpoints toward `LINK_REST`
///   - centering: weak pull toward viewport center
///   - damping: vx, vy *= DAMPING
/// Settled state is reached in ~150 ticks; we run 30 fps until
/// total kinetic energy drops below `STOP_KE`, then stop the rAF
/// loop until pan/drag/data change.

type NodeKind = "page" | "source";

type Node = {
  id: string;
  label: string;
  size: number;
  kind: NodeKind;
};

type Edge = {
  source: string;
  target: string;
};

type SimNode = Node & {
  x: number;
  y: number;
  vx: number;
  vy: number;
  radius: number;
};

interface Props {
  kmsName: string;
  onClose: () => void;
  onOpenFile: (target: ViewerTarget) => void;
}

const REPULSION = 12000; // node-node repel strength
const LINK_REST = 110; // ideal edge length (px)
const LINK_K = 0.04; // spring stiffness
const CENTER_K = 0.012; // pull-to-center strength
const DAMP_HOT = 0.85; // initial damping (loose, lets things spread)
const DAMP_COLD = 0.55; // final damping after anneal (kills motion fast)
const ANNEAL_TICKS = 100; // ticks to ramp from DAMP_HOT → DAMP_COLD
const STOP_AVG_KE = 0.4; // mean per-node KE below which sim halts
const MAX_TICKS = 200; // safety stop (~3.3s at 60fps)

export function KmsGraphView({ kmsName, onClose, onOpenFile }: Props) {
  const [nodes, setNodes] = useState<Node[] | null>(null);
  const [edges, setEdges] = useState<Edge[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [hover, setHover] = useState<string | null>(null);
  const [view, setView] = useState({ tx: 0, ty: 0, scale: 1 });
  const [includeSources, setIncludeSources] = useState(true);
  const [_, force] = useState(0); // re-render trigger from rAF loop

  const svgRef = useRef<SVGSVGElement | null>(null);
  const simRef = useRef<SimNode[]>([]);
  const rafRef = useRef<number | null>(null);
  const tickCount = useRef(0);
  const dragRef = useRef<{
    kind: "pan" | "node";
    nodeId?: string;
    startX: number;
    startY: number;
    lastX: number;
    lastY: number;
  } | null>(null);
  const sizeRef = useRef({ w: 800, h: 600 });

  // Fetch graph on mount / kms or includeSources change.
  // Result envelope echoes back `include_sources` so we ignore stale
  // responses if the user toggled mid-flight.
  useEffect(() => {
    setNodes(null);
    setEdges([]);
    setError(null);
    const unsub = subscribe((msg) => {
      if (
        msg.type === "kms_graph_result" &&
        (msg.kms as string) === kmsName &&
        Boolean(msg.include_sources) === includeSources
      ) {
        if (msg.ok) {
          setNodes((msg.nodes as Node[]) ?? []);
          setEdges((msg.edges as Edge[]) ?? []);
          setError(null);
        } else {
          setError((msg.error as string) ?? "graph failed");
          setNodes([]);
        }
      }
    });
    send({
      type: "kms_graph",
      name: kmsName,
      include_sources: includeSources,
    });
    return unsub;
  }, [kmsName, includeSources]);

  // Initialize simulation positions when nodes arrive (or change).
  // Spread on a circle so the force layout has somewhere sensible
  // to relax from.
  useEffect(() => {
    if (!nodes) return;
    const w = sizeRef.current.w;
    const h = sizeRef.current.h;
    const cx = w / 2;
    const cy = h / 2;
    const r = Math.min(w, h) * 0.35;
    // Preserve sim positions across re-fetches (toggling sources)
    // so the layout doesn't snap. Existing nodes keep (x, y) and
    // start with zero velocity. New nodes spawn near a connected,
    // already-placed neighbor when possible — that's the cheap fix
    // for the "graph thrashes for seconds after toggle" problem,
    // because edge springs are already near rest length.
    const prev = new Map(simRef.current.map((s) => [s.id, s]));
    const adjacency = new Map<string, string[]>();
    for (const e of edges) {
      (adjacency.get(e.source) ?? adjacency.set(e.source, []).get(e.source)!).push(e.target);
      (adjacency.get(e.target) ?? adjacency.set(e.target, []).get(e.target)!).push(e.source);
    }
    simRef.current = nodes.map((n, i) => {
      const radius = 2 + Math.min(4, n.size * 0.4);
      const carry = prev.get(n.id);
      if (carry) {
        return { ...n, x: carry.x, y: carry.y, vx: 0, vy: 0, radius };
      }
      // Try to anchor a new node next to a previously-placed neighbor
      // so the spring is already near its rest length.
      const neighbors = adjacency.get(n.id) ?? [];
      const placed = neighbors
        .map((id) => prev.get(id))
        .find((s): s is SimNode => s !== undefined);
      if (placed) {
        const jitter = LINK_REST * 0.5;
        const ang = Math.random() * Math.PI * 2;
        return {
          ...n,
          x: placed.x + Math.cos(ang) * jitter,
          y: placed.y + Math.sin(ang) * jitter,
          vx: 0,
          vy: 0,
          radius,
        };
      }
      // No neighbor (orphan source / first paint): seed circle.
      const theta = (i / Math.max(1, nodes.length)) * Math.PI * 2;
      return {
        ...n,
        x: cx + r * Math.cos(theta),
        y: cy + r * Math.sin(theta),
        vx: 0,
        vy: 0,
        radius,
      };
    });
    tickCount.current = 0;
    startSim();
    return () => stopSim();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [nodes]);

  // ESC closes.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  // Track svg size for centering.
  useEffect(() => {
    const el = svgRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      const rect = el.getBoundingClientRect();
      sizeRef.current = { w: rect.width || 800, h: rect.height || 600 };
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  function startSim() {
    if (rafRef.current !== null) return;
    const tick = () => {
      const sim = simRef.current;
      const n = sim.length;
      if (n === 0) {
        rafRef.current = null;
        return;
      }
      const cx = sizeRef.current.w / 2;
      const cy = sizeRef.current.h / 2;
      let ke = 0;

      // The currently-dragged node is "pinned" — it exerts force
      // on its neighbors (so they react), but receives no force and
      // doesn't integrate. The mouse owns its position outright.
      const pinnedId =
        dragRef.current?.kind === "node" ? dragRef.current.nodeId : undefined;

      // Repulsion (O(n²) — fine for ≤ ~100 nodes).
      for (let i = 0; i < n; i++) {
        for (let j = i + 1; j < n; j++) {
          const a = sim[i];
          const b = sim[j];
          const dx = b.x - a.x;
          const dy = b.y - a.y;
          const distSq = Math.max(64, dx * dx + dy * dy);
          const dist = Math.sqrt(distSq);
          const f = REPULSION / distSq;
          const fx = (f * dx) / dist;
          const fy = (f * dy) / dist;
          if (a.id !== pinnedId) {
            a.vx -= fx;
            a.vy -= fy;
          }
          if (b.id !== pinnedId) {
            b.vx += fx;
            b.vy += fy;
          }
        }
      }

      // Attraction along edges (Hooke spring toward LINK_REST).
      const idx = new Map(sim.map((s, i) => [s.id, i]));
      for (const e of edges) {
        const ai = idx.get(e.source);
        const bi = idx.get(e.target);
        if (ai === undefined || bi === undefined) continue;
        const a = sim[ai];
        const b = sim[bi];
        const dx = b.x - a.x;
        const dy = b.y - a.y;
        const dist = Math.sqrt(dx * dx + dy * dy) || 1;
        const f = LINK_K * (dist - LINK_REST);
        const fx = (f * dx) / dist;
        const fy = (f * dy) / dist;
        if (a.id !== pinnedId) {
          a.vx += fx;
          a.vy += fy;
        }
        if (b.id !== pinnedId) {
          b.vx -= fx;
          b.vy -= fy;
        }
      }

      // Center pull + damping + integrate. Damping anneals from
      // DAMP_HOT toward DAMP_COLD over `ANNEAL_TICKS` so the sim
      // converges aggressively after the initial spread phase —
      // many-body repulsion otherwise pumps energy indefinitely.
      const t = Math.min(1, tickCount.current / ANNEAL_TICKS);
      const damp = DAMP_HOT + (DAMP_COLD - DAMP_HOT) * t;
      for (const s of sim) {
        if (s.id === pinnedId) {
          // Pinned node: kill velocity, freeze position. The mouse
          // moves it directly via onPointerMove.
          s.vx = 0;
          s.vy = 0;
          continue;
        }
        s.vx += (cx - s.x) * CENTER_K;
        s.vy += (cy - s.y) * CENTER_K;
        s.vx *= damp;
        s.vy *= damp;
        s.x += s.vx;
        s.y += s.vy;
        ke += s.vx * s.vx + s.vy * s.vy;
      }

      tickCount.current += 1;
      force((t) => t + 1);
      // Compare AVERAGE per-node KE so the threshold doesn't scale
      // with node count — total KE goes up with N and would otherwise
      // keep the sim running long after the layout stopped moving
      // visibly. Min-tick floor avoids stopping during the first
      // frames where everything starts at v=0 and ke=0.
      const avgKe = ke / Math.max(1, n);
      if (
        (tickCount.current > 8 && avgKe < STOP_AVG_KE) ||
        tickCount.current > MAX_TICKS
      ) {
        rafRef.current = null;
        return;
      }
      rafRef.current = requestAnimationFrame(tick);
    };
    rafRef.current = requestAnimationFrame(tick);
  }
  function stopSim() {
    if (rafRef.current !== null) {
      cancelAnimationFrame(rafRef.current);
      rafRef.current = null;
    }
  }

  // Pointer handling: drag empty space → pan; drag node → reposition
  // (and re-energize the sim so neighbors react).
  function onPointerDown(e: React.PointerEvent) {
    const target = e.target as Element;
    const nodeId = target.getAttribute("data-node-id");
    const base = {
      startX: e.clientX,
      startY: e.clientY,
      lastX: e.clientX,
      lastY: e.clientY,
    };
    if (nodeId) {
      dragRef.current = { kind: "node", nodeId, ...base };
    } else {
      dragRef.current = { kind: "pan", ...base };
    }
    (e.target as Element).setPointerCapture?.(e.pointerId);
  }
  function onPointerMove(e: React.PointerEvent) {
    const d = dragRef.current;
    if (!d) {
      // hover-only path
      const target = e.target as Element;
      const nodeId = target.getAttribute("data-node-id");
      setHover(nodeId);
      return;
    }
    const dx = e.clientX - d.lastX;
    const dy = e.clientY - d.lastY;
    d.lastX = e.clientX;
    d.lastY = e.clientY;
    if (d.kind === "pan") {
      setView((v) => ({ ...v, tx: v.tx + dx, ty: v.ty + dy }));
    } else if (d.kind === "node" && d.nodeId) {
      const sim = simRef.current;
      const i = sim.findIndex((s) => s.id === d.nodeId);
      if (i >= 0) {
        sim[i].x += dx / view.scale;
        sim[i].y += dy / view.scale;
        sim[i].vx = 0;
        sim[i].vy = 0;
        tickCount.current = 0;
        startSim();
      }
    }
  }
  function onPointerUp(e: React.PointerEvent) {
    const d = dragRef.current;
    dragRef.current = null;
    if (d?.kind === "node" && d.nodeId) {
      // Distance is measured against the pointerdown position, NOT
      // the most-recent pointermove — otherwise every drag looks
      // like a click of <1px and the viewer opens (closing the
      // graph via App's mutually-exclusive switch).
      const moved = Math.hypot(e.clientX - d.startX, e.clientY - d.startY);
      if (moved < 3) {
        // treat as click; route source nodes (id prefixed with
        // `source:`) to the sources/ dir so the viewer reads from
        // the right place.
        const nid = d.nodeId;
        if (nid.startsWith("source:")) {
          const stem = nid.slice("source:".length);
          onOpenFile({ kms: kmsName, kind: "source", name: `${stem}.md` });
        } else {
          onOpenFile({ kms: kmsName, kind: "page", name: `${nid}.md` });
        }
      }
    }
  }
  function onWheel(e: React.WheelEvent) {
    e.preventDefault();
    const delta = -e.deltaY * 0.0015;
    setView((v) => {
      const newScale = Math.max(0.25, Math.min(3, v.scale * (1 + delta)));
      // Zoom around cursor.
      const svg = svgRef.current!;
      const rect = svg.getBoundingClientRect();
      const px = e.clientX - rect.left;
      const py = e.clientY - rect.top;
      const k = newScale / v.scale;
      return {
        scale: newScale,
        tx: px - k * (px - v.tx),
        ty: py - k * (py - v.ty),
      };
    });
  }

  // Memoize edge rendering by current sim positions; cheap enough.
  const sim = simRef.current;
  const idx = useMemo(() => new Map(sim.map((s, i) => [s.id, i])), [sim, _]);
  const isLinked = (id: string): boolean => {
    if (!hover) return false;
    if (id === hover) return true;
    return edges.some(
      (e) =>
        (e.source === hover && e.target === id) ||
        (e.target === hover && e.source === id),
    );
  };

  return (
    <div
      className="absolute inset-0 flex flex-col z-30 select-none"
      style={{ background: "var(--bg-primary)", WebkitUserSelect: "none" }}
    >
      <div
        className="flex items-center justify-between px-3 py-2 border-b shrink-0"
        style={{
          borderColor: "var(--border)",
          background: "var(--bg-secondary)",
        }}
      >
        <div
          className="flex items-center gap-2 text-xs"
          style={{ color: "var(--text-secondary)" }}
        >
          <Network size={13} />
          <span>
            Graph · <strong style={{ color: "var(--text-primary)" }}>{kmsName}</strong>
          </span>
          {nodes && (
            <span style={{ opacity: 0.6 }}>
              {nodes.length} {nodes.length === 1 ? "node" : "nodes"} ·{" "}
              {edges.length} {edges.length === 1 ? "edge" : "edges"}
            </span>
          )}
        </div>
        <div className="flex items-center gap-3">
          <label
            className="flex items-center gap-1.5 text-[10px] cursor-pointer select-none"
            style={{ color: "var(--text-secondary)" }}
            title="Toggle source archive nodes + citation edges"
          >
            <input
              type="checkbox"
              checked={includeSources}
              onChange={(e) => setIncludeSources(e.target.checked)}
              className="cursor-pointer"
              style={{ accentColor: "var(--accent, #61afef)" }}
            />
            <span>Include sources</span>
          </label>
          <div
            className="text-[10px]"
            style={{ color: "var(--text-secondary)", opacity: 0.7 }}
          >
            drag · wheel · click · ESC
          </div>
          <button
            type="button"
            onClick={onClose}
            className="p-0.5 rounded hover:bg-white/10"
            style={{ color: "var(--text-secondary)" }}
            title="Close graph"
          >
            <X size={14} />
          </button>
        </div>
      </div>

      <div className="flex-1 relative overflow-hidden">
        {error && (
          <div
            className="absolute inset-0 flex items-center justify-center text-xs"
            style={{ color: "var(--danger, #e06c75)" }}
          >
            {error}
          </div>
        )}
        {!error && nodes !== null && nodes.length === 0 && (
          <div
            className="absolute inset-0 flex items-center justify-center text-xs italic"
            style={{ color: "var(--text-secondary)" }}
          >
            No pages in this KMS yet.
          </div>
        )}
        {nodes === null && !error && (
          <div
            className="absolute inset-0 flex items-center justify-center text-xs italic"
            style={{ color: "var(--text-secondary)" }}
          >
            Building graph…
          </div>
        )}
        <svg
          ref={svgRef}
          className="w-full h-full"
          style={{
            cursor: dragRef.current?.kind === "pan" ? "grabbing" : "default",
          }}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerLeave={() => setHover(null)}
          onWheel={onWheel}
        >
          <g transform={`translate(${view.tx},${view.ty}) scale(${view.scale})`}>
            {edges.map((e, i) => {
              const ai = idx.get(e.source);
              const bi = idx.get(e.target);
              if (ai === undefined || bi === undefined) return null;
              const a = sim[ai];
              const b = sim[bi];
              const dim = hover && !(e.source === hover || e.target === hover);
              return (
                <line
                  key={i}
                  x1={a.x}
                  y1={a.y}
                  x2={b.x}
                  y2={b.y}
                  stroke="var(--text-secondary)"
                  strokeOpacity={dim ? 0.1 : 0.45}
                  strokeWidth={1}
                />
              );
            })}
            {sim.map((s) => {
              const linked = isLinked(s.id);
              const dim = hover !== null && !linked;
              // Source nodes render as smaller, muted, square-ish
              // markers (rotated diamond) so they read distinctly
              // from page nodes without needing a legend. The Obsidian
              // convention is "attachment = different color/shape".
              const isSrc = s.kind === "source";
              const fill =
                hover === s.id
                  ? "var(--accent, #61afef)"
                  : isSrc
                    ? "var(--text-secondary)"
                    : "var(--text-primary)";
              return (
                <g key={s.id} opacity={dim ? 0.3 : 1}>
                  {isSrc ? (
                    <rect
                      data-node-id={s.id}
                      x={s.x - s.radius * 0.85}
                      y={s.y - s.radius * 0.85}
                      width={s.radius * 1.7}
                      height={s.radius * 1.7}
                      transform={`rotate(45 ${s.x} ${s.y})`}
                      fill={fill}
                      stroke="var(--bg-primary)"
                      strokeWidth={1.5}
                      style={{ cursor: "pointer" }}
                    />
                  ) : (
                    <circle
                      data-node-id={s.id}
                      cx={s.x}
                      cy={s.y}
                      r={s.radius}
                      fill={fill}
                      stroke="var(--bg-primary)"
                      strokeWidth={1.5}
                      style={{ cursor: "pointer" }}
                    />
                  )}
                  <text
                    x={s.x}
                    y={s.y + s.radius + 7}
                    textAnchor="middle"
                    fontSize={7.5}
                    fill={
                      isSrc
                        ? "var(--text-secondary)"
                        : "var(--text-primary)"
                    }
                    style={{ pointerEvents: "none", userSelect: "none" }}
                  >
                    {s.label.length > 28
                      ? s.label.slice(0, 27) + "…"
                      : s.label}
                  </text>
                </g>
              );
            })}
          </g>
        </svg>
      </div>
    </div>
  );
}
