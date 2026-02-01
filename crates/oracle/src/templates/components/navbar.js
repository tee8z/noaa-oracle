// Navbar hamburger menu toggle for mobile
(function () {
  function initNavbarBurgers() {
    const navbarBurgers = Array.prototype.slice.call(
      document.querySelectorAll(".navbar-burger"),
      0,
    );

    navbarBurgers.forEach((burger) => {
      // Prevent duplicate listeners by marking initialized burgers
      if (burger.dataset.initialized) return;
      burger.dataset.initialized = "true";

      burger.addEventListener("click", () => {
        const targetId = burger.dataset.target;
        const target = document.getElementById(targetId);

        burger.classList.toggle("is-active");
        target.classList.toggle("is-active");
      });
    });
  }

  // Initialize on page load (handles both early and late script loading)
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", initNavbarBurgers);
  } else {
    initNavbarBurgers();
  }

  // Re-initialize after HTMX swaps
  document.addEventListener("htmx:afterSwap", initNavbarBurgers);

  // Update active navbar item based on current URL
  function updateActiveNavItem() {
    const path = window.location.pathname;
    const navItems = document.querySelectorAll(".navbar-menu .navbar-item");
    navItems.forEach((item) => {
      const href = item.getAttribute("href");
      if (!href) return;
      const isActive =
        (href === "/" && (path === "/" || path === "")) ||
        (href !== "/" && path.startsWith(href));
      item.classList.toggle("is-active", isActive);
    });
  }

  // Update active state after HTMX navigation
  document.addEventListener("htmx:pushedIntoHistory", updateActiveNavItem);
  document.addEventListener("htmx:replacedInHistory", updateActiveNavItem);

  // Close mobile menu when clicking a nav link (use event delegation)
  document.addEventListener("click", (event) => {
    const navItem = event.target.closest(".navbar-item");
    if (navItem) {
      const navbar = document.querySelector(".navbar-menu.is-active");
      const burger = document.querySelector(".navbar-burger.is-active");
      if (navbar && burger) {
        navbar.classList.remove("is-active");
        burger.classList.remove("is-active");
      }
    }
  });
})();
