import { parseTraceForReplay, createTraceFromEvents, TraceStep, TraceReplayData } from '../../services/trace_parser';

describe('Trace Parser', () => {
  const sampleFlamegraphTrace = {
    version: '0.0.1',
    contractId: '0x1234567890abcdef',
    name: 'Multi-contract invocation',
    instructionCount: 50000,
    gasUsed: 45000,
    v: {
      calls: [
        {
          contractId: '0xabcdef1234567890',
          name: 'ContractA',
          instructionCount: 25000,
          gasUsed: 22000,
        },
        {
          contractId: '0x1111111111111111',
          name: 'ContractB',
          instructionCount: 15000,
          gasUsed: 13000,
        },
      ],
    },
  };

  const sampleEventStream = [
    {
      id: 'evt-1',
      timestamp: '2024-01-01T00:00:00Z',
      ledger: 100,
      event_type: 'contract',
      topics: ['Transfer'],
      value_xdr: '0x1234',
      tx_hash: '0xabc123',
    },
    {
      id: 'evt-2',
      timestamp: '2024-01-01T00:00:01Z',
      ledger: 101,
      event_type: 'contract',
      topics: ['Transfer'],
      value_xdr: '0x5678',
      tx_hash: '0xabc123',
    },
  ];

  describe('parseTraceForReplay', () => {
    it('parses flamegraph trace into structured TraceReplayData', () => {
      const result = parseTraceForReplay(sampleFlamegraphTrace);

      expect(result).toBeDefined();
      expect(result.traceId).toBe('0x1234567890abcdef');
      expect(result.txHash).toBe('0x1234567890abcdef');
      expect(result.steps.length).toBe(3); // main + 2 subcalls
      expect(result.steps[0].eventName).toBe('Multi-contract invocation');
      expect(result.steps[0].instructionCount).toBe(50000);
      expect(result.steps[0].eventType).toBe('contract');
      expect(result.finalLedgerState.ledgerSeq).toBe(101);
      expect(result.description).toBe('Multi-contract invocation');
    });

    it('handles empty trace data', () => {
      const result = parseTraceForReplay({});
      expect(result.steps.length).toBe(0);
      expect(result.finalLedgerState.ledgerSeq).toBe(0);
    });

    it('parses trace with nested subcalls', () => {
      const traceWithDeepNested = {
        ...sampleFlamegraphTrace,
        v: {
          calls: [
            {
              contractId: '0xabcdef1234567890',
              name: 'ContractA',
              instructionCount: 25000,
              v: {
                calls: [
                  {
                    contractId: '0x0000000000000001',
                    name: 'SubContract',
                    instructionCount: 5000,
                  },
                ],
              },
            },
          ],
        },
      };
      const result = parseTraceForReplay(traceWithDeepNested);
      expect(result.steps.length).toBeGreaterThan(1);
    });
  });

  describe('createTraceFromEvents', () => {
    it('creates TraceReplayData from event stream', () => {
      const result = createTraceFromEvents(sampleEventStream);

      expect(result).toBeDefined();
      expect(result.traceId).toBe('0xabc123');
      expect(result.txHash).toBe('0xabc123');
      expect(result.steps.length).toBe(2);
      expect(result.steps[0].eventName).toBe('Transfer');
      expect(result.steps[0].eventType).toBe('contract');
      expect(result.steps[0].ledgerSeq).toBe(100);
      expect(result.steps[1].ledgerSeq).toBe(101);
    });

    it('handles empty event stream', () => {
      const result = createTraceFromEvents([]);
      expect(result.steps.length).toBe(0);
      expect(result.traceId).toBe('');
    });
  });
});