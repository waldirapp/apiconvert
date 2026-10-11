// A seleção invalida o resultado anterior, inclusive respostas ainda em trânsito.
const versoes = new WeakMap();

function formularioEnvio(elemento) {
  return elemento instanceof Element ? elemento.closest("form.envio") : null;
}

function mostrarEstado(form, mensagem) {
  const resultado = document.querySelector(form.getAttribute("hx-target"));
  if (resultado) resultado.textContent = mensagem;
}

document.addEventListener("change", (ev) => {
  const input = ev.target;
  if (!(input instanceof HTMLInputElement) || input.type !== "file") return;
  const form = formularioEnvio(input);
  if (form) {
    versoes.set(form, (versoes.get(form) || 0) + 1);
    form.dispatchEvent(new Event("htmx:abort"));
    mostrarEstado(form, input.files.length
      ? "Arquivo selecionado. Clique em Converter para carregar o novo resultado."
      : "");
  }
  if (!input.dataset.previa) return;
  const img = document.getElementById(input.dataset.previa);
  const arquivo = input.files && input.files[0];
  if (!img) return;
  if (img.src.startsWith("blob:")) URL.revokeObjectURL(img.src);
  if (arquivo && arquivo.type.startsWith("image/")) {
    img.src = URL.createObjectURL(arquivo);
    img.hidden = false;
  } else {
    img.removeAttribute("src");
    img.hidden = true;
  }
});

// htmx 4 usa nomes de eventos com dois-pontos e o contexto em detail.ctx.
document.addEventListener("htmx:config:request", (ev) => {
  const ctx = ev.detail.ctx;
  const form = formularioEnvio(ctx.sourceElement);
  if (!form) return;
  ctx.versaoArquivo = versoes.get(form) || 0;
});

document.addEventListener("htmx:before:request", (ev) => {
  const ctx = ev.detail.ctx;
  const form = formularioEnvio(ctx.sourceElement);
  if (!form) return;
  if (ctx.versaoArquivo !== (versoes.get(form) || 0)) {
    ev.preventDefault();
    return;
  }
  mostrarEstado(form, "Convertendo novo arquivo…");
});

document.addEventListener("htmx:before:swap", (ev) => {
  const ctx = ev.detail.ctx;
  const form = formularioEnvio(ctx.sourceElement);
  if (form && ctx.versaoArquivo !== (versoes.get(form) || 0)) ev.preventDefault();
});

document.addEventListener("htmx:error", (ev) => {
  const ctx = ev.detail.ctx;
  if (!ctx) return;
  const form = formularioEnvio(ctx.sourceElement);
  if (form && ctx.versaoArquivo === (versoes.get(form) || 0)) {
    mostrarEstado(form, "Não foi possível carregar o resultado. Tente converter novamente.");
  }
});

document.addEventListener("dragover", (ev) => {
  const alvo = ev.target instanceof Element ? ev.target.closest("label.arquivo") : null;
  if (alvo) ev.preventDefault();
});

document.addEventListener("drop", (ev) => {
  const alvo = ev.target instanceof Element ? ev.target.closest("label.arquivo") : null;
  const input = alvo && alvo.querySelector('input[type="file"]');
  if (!input || !ev.dataTransfer || !ev.dataTransfer.files.length) return;
  ev.preventDefault();
  const arquivos = new DataTransfer();
  arquivos.items.add(ev.dataTransfer.files[0]);
  input.files = arquivos.files;
  input.dispatchEvent(new Event("change", { bubbles: true }));
});

document.addEventListener("click", async (ev) => {
  const botao = ev.target instanceof Element ? ev.target.closest("[data-copiar]") : null;
  if (!botao) return;
  const area = botao.parentElement && botao.parentElement.querySelector("textarea");
  if (!area) return;
  try {
    await navigator.clipboard.writeText(area.value);
    botao.textContent = "Copiado";
  } catch {
    area.select();
    botao.textContent = "Selecionado: use Ctrl+C";
  }
});
