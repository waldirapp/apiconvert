// npm install --no-save playwright; npx playwright install chromium
// node tests/ui_state.cjs http://127.0.0.1:9098
const { chromium } = require("playwright");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const assert = require("node:assert/strict");

(async () => {
  const browser = await chromium.launch({ headless: true });
  try {
    for (const tela of ["planilha", "imagem"]) {
      const page = await browser.newPage();
      if (!process.argv.includes("--deployed")) await page.route("**/static/app.js*", route => route.fulfill({
        contentType: "text/javascript",
        body: readFileSync(join(__dirname, "../assets/app.js"), "utf8"),
      }));
      await page.goto(`${process.argv[2]}/${tela}`);
      await page.waitForFunction(() => document.querySelector("form.envio").hasAttribute("data-htmx-powered"));
      const input = page.locator('input[type="file"]');
      const file = tela === "imagem"
        ? { name: "new.png", mimeType: "image/png", buffer: Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg==", "base64") }
        : { name: "new.xls", mimeType: "application/vnd.ms-excel", buffer: Buffer.from("fixture") };
      await page.locator("#resultado").evaluate(el => el.innerHTML = '<a href="/old">Old download</a><p>Old result</p>');
      await input.setInputFiles(file);
      assert.match(await page.locator("#resultado").innerText(), /Arquivo selecionado/);
      assert.equal(await page.locator("#resultado a").count(), 0);
      if (tela === "imagem") assert.equal(await page.locator("#previa-original").isVisible(), true);

      // Ignora deliberadamente AbortSignal para exercitar a proteção contra
      // resposta antiga mesmo quando o transporte termina depois da seleção.
      await page.evaluate(() => {
        const fetchOriginal = window.fetch;
        window.fetch = (url, options) => String(url).includes("/ui/")
          ? new Promise(resolve => window.finishConversion = html => resolve(new Response(html, { status: 200, headers: { "Content-Type": "text/html" } })))
          : fetchOriginal(url, options);
      });
      await page.locator("#resultado").evaluate(el => el.textContent = "Previous result");
      await page.getByRole("button", { name: "Converter", exact: true }).click();
      await page.waitForFunction(() => document.querySelector("#resultado").textContent === "Convertendo novo arquivo…");
      await page.evaluate(() => window.finishConversion('<p id="current-result">New result</p>'));
      await page.locator("#current-result").waitFor();

      await page.getByRole("button", { name: "Converter", exact: true }).click();
      await page.waitForFunction(() => document.querySelector("#resultado").textContent === "Convertendo novo arquivo…");
      await input.setInputFiles({ ...file, name: "another.xls" });
      const completed = page.waitForEvent("console", msg => msg.text() === "request-finished");
      await page.evaluate(() => {
        document.addEventListener("htmx:finally:request", () => console.log("request-finished"), { once: true });
        window.finishConversion('<p id="stale-result">Stale result</p>');
      });
      await completed;
      assert.equal(await page.locator("#stale-result").count(), 0);
      assert.match(await page.locator("#resultado").innerText(), /Arquivo selecionado/);

      await page.locator("#resultado").evaluate(el => el.textContent = "Old drop result");
      await page.locator("label.arquivo span").evaluate(el => {
        const transfer = new DataTransfer();
        transfer.items.add(new File(["fixture"], "dropped.xls"));
        el.dispatchEvent(new DragEvent("drop", { dataTransfer: transfer, bubbles: true, cancelable: true }));
      });
      assert.match(await page.locator("#resultado").innerText(), /Arquivo selecionado/);
      assert.equal(await input.evaluate(el => el.files[0].name), "dropped.xls");
      await input.setInputFiles([]);
      assert.equal(await page.locator("#resultado").innerText(), "");
      if (tela === "imagem") assert.equal(await page.locator("#previa-original").isVisible(), false);
      console.log(`PASS ${tela}: seleção, importação, resposta antiga, arrastar e remoção`);
      await page.close();
    }
  } finally {
    await browser.close();
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
