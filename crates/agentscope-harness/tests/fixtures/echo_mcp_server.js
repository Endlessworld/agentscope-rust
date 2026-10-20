// Minimal self-contained MCP stdio server (no external deps).
// Speaks JSON-RPC 2.0 over stdin/stdout. Exposes one tool "echo_message".
const readline = require('readline');

const rl = readline.createInterface({ input: process.stdin, terminal: false });
let idCounter = 0;

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

function handle(method, params, id) {
  switch (method) {
    case 'initialize':
      send({
        jsonrpc: '2.0', id,
        result: {
          protocolVersion: '2024-11-05',
          capabilities: { tools: {} },
          serverInfo: { name: 'echo-server', version: '1.0.0' },
        },
      });
      break;
    case 'notifications/initialized':
      break;
    case 'tools/list':
      send({
        jsonrpc: '2.0', id,
        result: {
          tools: [
            {
              name: 'echo_message',
              description: 'Echoes the provided message back',
              inputSchema: {
                type: 'object',
                properties: { message: { type: 'string' } },
                required: ['message'],
              },
            },
          ],
        },
      });
      break;
    case 'tools/call':
      const args = (params && params.arguments) || {};
      const msg = args.message || '';
      // Echo the propagated request metadata so tests can assert on the wire.
      const meta = (params && params._meta) || null;
      const text = meta === null
        ? `echo: ${msg}`
        : `echo: ${msg} | meta: ${JSON.stringify(meta)}`;
      send({
        jsonrpc: '2.0', id,
        result: {
          content: [{ type: 'text', text }],
        },
      });
      break;
    default:
      send({ jsonrpc: '2.0', id, error: { code: -32601, message: `Method not found: ${method}` } });
  }
}

rl.on('line', (line) => {
  try {
    const req = JSON.parse(line);
    if (req.id !== undefined) {
      handle(req.method, req.params, req.id);
    }
  } catch (e) {
    // ignore malformed lines
  }
});