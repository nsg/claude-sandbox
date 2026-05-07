const http = require("http");
const fs = require("fs");

const html = fs.readFileSync("/tmp/hub-bootstrap.html");

const server = http.createServer((req, res) => {
  if (req.method === "POST" && req.url === "/bootstrap-done") {
    res.writeHead(200);
    res.end();
    fs.writeFileSync("/tmp/bootstrap-done", "");
    setTimeout(() => {
      server.close();
      process.exit(0);
    }, 200);
    return;
  }
  res.writeHead(200, { "Content-Type": "text/html" });
  res.end(html);
});

server.listen(parseInt(process.env.PORT), "0.0.0.0", () => {
  fs.writeFileSync("/tmp/bootstrap-ready", "");
});
