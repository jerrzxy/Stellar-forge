import { useState, useRef, useCallback, useEffect } from 'react';
import {
  validateAuthTree,
  generateAuthPayload,
  createAliceRouterLiquidityPoolTree,
} from '../lib/auth_validator';

/**
 * @typedef {import('../lib/auth_validator').AuthTreeNode} AuthTreeNode
 * @typedef {import('../lib/auth_validator').AuthPayload} AuthPayload
 */

const NODE_WIDTH = 130;
const NODE_HEIGHT = 44;
const LEVEL_X_OFFSET = 180;
const LEVEL_Y_SPACING = 64;
const NODE_RADIUS = 8;

/** TreeView — drag-and-drop tree component for building Soroban auth hierarchies.
 *
 * Supports:
 *   - Arbitrary tree depth (each contract node can have sub-contracts),
 *   - Zoom (mouse wheel) and pan (drag on empty canvas) for large graphs,
 *   - Node drag to reposition,
 *   - One-click validation + JSON payload generation for soroban-cli.
 */
export default function TreeView() {
  const [nodes, setNodes] = useState(defaultAliceRouterTree);
  const [selectedId, setSelectedId] = useState(null);
  const [zoom, setZoom] = useState(1);
  const [pan, setPan] = useState({ x: 40, y: 40 });
  const [dragging, setDragging] = useState(null);
  const [panning, setPanning] = useState(null);

  const svgRef = useRef(null);
  const containerRef = useRef(null);
  const positionsRef = useRef(new Map());

  // Recompute layout whenever the tree shape or transform changes.
  useEffect(() => {
    positionsRef.current = computeTreeLayout(nodes, pan.x, pan.y, zoom);
  }, [nodes, pan, zoom]);

  const selectedNode = nodes.find((n) => n.id === selectedId) ?? null;

  const zoomBy = useCallback((factor) => {
    setZoom((z) => Math.max(0.25, Math.min(4, z * factor)));
  }, []);

  const handleWheel = useCallback(
    (e) => {
      e.preventDefault();
      const factor = e.deltaY > 0 ? 0.9 : 1.1;
      zoomBy(factor);
    },
    [zoomBy],
  );

  const handlePanStart = useCallback(
    (e) => {
      // Don't start pan when clicking a node.
      if (e.target.closest('[data-node]')) return;
      setPanning({
        startX: e.clientX,
        startY: e.clientY,
        panX: pan.x,
        panY: pan.y,
      });
    },
    [pan],
  );

  const handlePanMove = useCallback(
    (e) => {
      if (!panning) return;
      const dx = e.clientX - panning.startX;
      const dy = e.clientY - panning.startY;
      setPan({ x: panning.panX + dx, y: panning.panY + dy });
    },
    [panning],
  );

  const handlePanEnd = useCallback(() => {
    setPanning(null);
  }, []);

  const handleNodeMouseDown = useCallback(
    (e, id, x, y) => {
      e.stopPropagation();
      setSelectedId(id);
      setDragging({ id, offsetX: e.clientX - x, offsetY: e.clientY - y });
    },
    [],
  );

  // Node drag lives outside React to keep pointer smooth.
  useEffect(() => {
    if (!dragging) return;
    const onMove = (e) => {
      const newPan = { x: pan.x, y: pan.y };
      const updated = new Map(positionsRef.current);
      for (const [nid, pos] of updated) {
        if (nid === dragging.id) {
          updated.set(nid, {
            x: e.clientX - dragging.offsetX - newPan.x,
            y: e.clientY - dragging.offsetY - newPan.y,
          });
        }
      }
      positionsRef.current = updated;
    };
    const onUp = () => setDragging(null);
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
    return () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
    };
  }, [dragging, pan]);

  // ---- tree editing ----
  const addSiblingOrLeaf = useCallback(() => {
    const parent = selectedNode ?? nodes[0];
    const nextId = `node-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
    const newNode = {
      id: nextId,
      label: `Address ${nodes.length + 1}`,
      type: 'address',
      address: makeAddress(),
      depth: (parent?.depth ?? 0) + 1,
    };
    setNodes((prev) =>
      prev.map((n) =>
        n.id === parent?.id
          ? { ...n, subContracts: [...(n.subContracts ?? []), newNode] }
          : n,
      ),
    );
    setSelectedId(nextId);
  }, [nodes, selectedNode]);

  const addChildToSelected = useCallback(() => {
    const parent = selectedNode;
    if (!parent) return;
    const nextId = `node-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
    const asContract = parent.type === 'address';
    const newNode = {
      id: nextId,
      label: asContract
        ? `Sub-contract ${parent.subContracts?.length ?? 0}`
        : `Address ${nodes.length + 1}`,
      type: asContract ? 'contract' : 'address',
      address: asContract ? undefined : makeAddress(),
      contractId: asContract
        ? `CCNTH${Math.random().toString(36).slice(2, 52).padEnd(55, 'X')}`
        : undefined,
      depth: (parent.depth ?? 0) + 1,
    };
    setNodes((prev) =>
      prev.map((n) =>
        n.id === parent.id
          ? { ...n, subContracts: [...(n.subContracts ?? []), newNode] }
          : n,
      ),
    );
    setSelectedId(nextId);
  }, [nodes, selectedNode]);

  const removeSelected = useCallback(() => {
    if (!selectedId) return;
    const removeRecursive = (list) =>
      list
        .filter((n) => n.id !== selectedId)
        .map((n) => ({
          ...n,
          subContracts: n.subContracts ? removeRecursive(n.subContracts) : undefined,
        }));
    setNodes((prev) => removeRecursive(prev));
    setSelectedId(null);
  }, [selectedId]);

  const handlePayload = useCallback(() => {
    const root = flattenToRoot(nodes);
    const payload = generateAuthPayload(root);
    if (!payload) {
      alert('Failed to generate valid auth payload. Check the tree structure.');
      return;
    }
    alert('Auth Payload Generated:\n' + JSON.stringify(payload, null, 2));
  }, [nodes]);

  const handleValidate = useCallback(() => {
    const root = flattenToRoot(nodes);
    const result = validateAuthTree(root);
    if (result.valid) {
      alert('Auth tree is valid!');
    } else {
      alert(
        `Auth tree has ${result.errors.length} error(s):\n${result.errors.join('\n')}`,
      );
    }
    if (result.payload) {
      alert('Generated Payload:\n' + JSON.stringify(result.payload, null, 2));
    }
  }, [nodes]);

  return (
    <div
      ref={containerRef}
      className="tree-view-container"
      style={{
        width: '100%',
        height: '800px',
        border: '1px solid #ddd',
        overflow: 'hidden',
        fontFamily: 'sans-serif',
        display: 'flex',
        flexDirection: 'column',
      }}
    >
      <header
        style={{
          padding: '8px 12px',
          background: '#f8f9fa',
          borderBottom: '1px solid #dee2e3',
          display: 'flex',
          alignItems: 'center',
          gap: '8px',
        }}
      >
        <h2 style={{ margin: 0 }}>Soroban Auth Hierarchy Builder</h2>
        <div style={{ marginLeft: 'auto', display: 'flex', gap: '6px' }}>
          <button onClick={addSiblingOrLeaf} title="Add sibling/leaf node">
            + Add
          </button>
          <button
            onClick={addChildToSelected}
            disabled={!selectedNode}
            title="Add child of selected node"
          >
            + Child
          </button>
          <button
            onClick={removeSelected}
            disabled={!selectedNode}
            title="Remove selected node"
          >
            − Remove
          </button>
          <span style={{ margin: '0 8px' }} />
          <button onClick={handleValidate}>Validate</button>
          <button onClick={handlePayload}>Generate JSON</button>
        </div>
      </header>

      {/* Canvas with zoom + pan */}
      <div
        style={{
          flex: 1,
          overflow: 'hidden',
          position: 'relative',
          cursor: panning ? 'grabbing' : 'grab',
        }}
        onWheel={handleWheel}
        onMouseDown={handlePanStart}
        onMouseMove={handlePanMove}
        onMouseUp={handlePanEnd}
        onMouseLeave={handlePanEnd}
      >
        <svg
          ref={svgRef}
          width="100%"
          height="100%"
          style={{ display: 'block', cursor: dragging ? 'grabbing' : 'default' }}
        >
          <g transform={`translate(${pan.x} ${pan.y}) scale(${zoom})`}>
            <TreeCanvas
              nodes={nodes}
              positions={positionsRef.current}
              onNodeMouseDown={handleNodeMouseDown}
              selectedId={selectedId}
            />
          </g>
        </svg>
      </div>
    </div>
  );
}

function makeAddress() {
  return (
    'G' +
    Array.from({ length: 55 }, () =>
      'ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567'[Math.floor(Math.random() * 32)],
    ).join('')
  );
}

/** Render the tree as SVG with links + draggable nodes. */
function TreeCanvas({ nodes, positions, onNodeMouseDown, selectedId }) {
  return (
    <>
      {linksFromTree(nodes, positions).map((d, i) => (
        <path key={`link-${i}`} d={d} fill="none" stroke="#cbd5e1" strokeWidth={2} />
      ))}
      {nodes.map((node) => {
        const pos = positions.get(node.id);
        if (!pos) return null;
        const isSelected = node.id === selectedId;
        return (
          <g
            key={node.id}
            data-node
            transform={`translate(${pos.x} ${pos.y})`}
            onMouseDown={(e) => onNodeMouseDown(e, node.id, pos.x, pos.y)}
            style={{ cursor: 'grab' }}
          >
            <rect
              width={NODE_WIDTH}
              height={NODE_HEIGHT}
              rx={NODE_RADIUS}
              ry={NODE_RADIUS}
              fill={node.type === 'contract' ? '#3b82f6' : '#10b981'}
              stroke={isSelected ? '#f59e0b' : '#fff'}
              strokeWidth={isSelected ? 3 : 2}
            />
            <text
              x={NODE_WIDTH / 2}
              y={NODE_HEIGHT / 2 + 4}
              textAnchor="middle"
              fill="#fff"
              fontSize={12}
              fontWeight={700}
            >
              {node.label}
            </text>
            {node.type === 'address' && node.address && (
              <text
                x={NODE_WIDTH / 2}
                y={NODE_HEIGHT + 12}
                textAnchor="middle"
                fill="#64748b"
                fontSize={9}
              >
                {node.address}
              </text>
            )}
            {node.type === 'contract' && node.contractId && (
              <text
                x={NODE_WIDTH / 2}
                y={NODE_HEIGHT + 12}
                textAnchor="middle"
                fill="#64748b"
                fontSize={9}
              >
                {node.contractId}
              </text>
            )}
          </g>
        );
      })}
    </>
  );
}

function linksFromTree(nodes, positions) {
  const lines = [];
  function walk(list) {
    for (const n of list) {
      const p = positions.get(n.id);
      if (!p) continue;
      for (const c of n.subContracts ?? []) {
        const cp = positions.get(c.id);
        if (cp) {
          lines.push(
            `M${p.x + NODE_WIDTH / 2} ${p.y + NODE_HEIGHT} L${cp.x + NODE_WIDTH / 2} ${cp.y}`,
          );
        }
        walk(c.subContracts ?? []);
      }
    }
  }
  walk(nodes);
  return lines;
}

function computeTreeLayout(nodes, panX, panY, zoom) {
  const out = new Map();
  function walk(list, x, y) {
    let childY = y;
    list.forEach((n) => {
      out.set(n.id, { x, y: childY });
      childY += NODE_HEIGHT + LEVEL_Y_SPACING;
      walk(n.subContracts ?? [], x + LEVEL_X_OFFSET / zoom, childY);
    });
  }
  walk(nodes, 40, 40);
  return out;
}

/** Wrap top-level array elements into a synthetic root so the validator
 * always receives a single root node. */
function flattenToRoot(nodes) {
  if (nodes.length === 1) return nodes[0];
  return {
    id: 'root-synthetic',
    label: 'Auth Tree Root',
    type: 'contract',
    contractId: 'root-contract',
    depth: 0,
    subContracts: nodes,
  };
}

function defaultAliceRouterTree() {
  return [createAliceRouterLiquidityPoolTree()];
}
