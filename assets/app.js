// Prévia local da imagem escolhida e cópia do base64. Sem dados enviados.
document.addEventListener("change", (ev) => {
  const input = ev.target;
  if (!(input instanceof HTMLInputElement) || !input.dataset.previa) return;
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
