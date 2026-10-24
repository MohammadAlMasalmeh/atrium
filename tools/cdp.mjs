// A small DevTools RPC client using Node's built-in WebSocket.
// Input is one JSON object on stdin; output is one JSON result on stdout.
let input = '';
for await (const chunk of process.stdin) input += chunk;
const { url, method, params = {} } = JSON.parse(input);
const socket = new WebSocket(url);
const timer = setTimeout(() => { console.error(`DevTools timeout: ${method}`); process.exit(1); }, 20000);
socket.addEventListener('open', () => socket.send(JSON.stringify({ id: 1, method, params })));
socket.addEventListener('message', ({ data }) => {
  const message = JSON.parse(data);
  if (message.id !== 1) return;
  clearTimeout(timer);
  socket.close();
  if (message.error) { console.error(JSON.stringify(message.error)); process.exitCode = 1; }
  else process.stdout.write(JSON.stringify(message.result));
});
socket.addEventListener('error', error => { clearTimeout(timer); console.error(String(error)); process.exit(1); });
