for (const b of document.querySelectorAll("button.copy")) {
  b.hidden = false;
  b.addEventListener("click", async () => {
    await navigator.clipboard.writeText(b.parentElement.querySelector("code").textContent);
    b.textContent = "copied";
    setTimeout(() => (b.textContent = "copy"), 1500);
  });
}
