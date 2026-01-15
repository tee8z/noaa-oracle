// Navbar hamburger menu toggle for mobile
document.addEventListener('DOMContentLoaded', () => {
    // Get all navbar burgers
    const navbarBurgers = Array.prototype.slice.call(
        document.querySelectorAll('.navbar-burger'),
        0
    );

    // Add click event to each burger
    navbarBurgers.forEach(burger => {
        burger.addEventListener('click', () => {
            // Get the target menu
            const targetId = burger.dataset.target;
            const target = document.getElementById(targetId);

            // Toggle the is-active class
            burger.classList.toggle('is-active');
            target.classList.toggle('is-active');
        });
    });
});

// Close mobile menu when clicking a nav link
document.addEventListener('click', (event) => {
    const navItem = event.target.closest('.navbar-item');
    if (navItem) {
        const navbar = document.querySelector('.navbar-menu.is-active');
        const burger = document.querySelector('.navbar-burger.is-active');
        if (navbar && burger) {
            navbar.classList.remove('is-active');
            burger.classList.remove('is-active');
        }
    }
});
