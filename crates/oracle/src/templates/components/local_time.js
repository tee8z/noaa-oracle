// Local time conversion for UTC timestamps
// Converts all elements with class "local-time" to user's local timezone

function convertToLocalTime() {
    const elements = document.querySelectorAll('.local-time[data-utc]');

    elements.forEach(el => {
        const utcString = el.getAttribute('data-utc');
        if (!utcString) return;

        try {
            const date = new Date(utcString);
            if (isNaN(date.getTime())) return;

            // Format as local date/time
            const options = {
                year: 'numeric',
                month: 'short',
                day: 'numeric',
                hour: '2-digit',
                minute: '2-digit',
                timeZoneName: 'short'
            };

            el.textContent = date.toLocaleString(undefined, options);
        } catch (e) {
            // Keep original value on error
            console.warn('Failed to convert time:', utcString, e);
        }
    });
}

// Initialize on page load
if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', convertToLocalTime);
} else {
    convertToLocalTime();
}

// Re-run after HTMX swaps (for SPA navigation and partial updates)
document.addEventListener('htmx:afterSwap', convertToLocalTime);
document.addEventListener('htmx:afterSettle', convertToLocalTime);
