/** @type {import('tailwindcss').Config} */
export default {
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      colors: {
        'bg-primary': '#0d0d0d',
        'bg-card': '#141414',
        'bg-card-hover': '#181818',
        'border-subtle': '#252525',
        'border-accent': '#333',
        'text-primary': '#e5e5e5',
        'text-secondary': '#737373',
        'text-muted': '#525252',
        'accent-green': '#22c55e',
        'accent-red': '#ef4444',
        'accent-blue': '#3b82f6',
        'accent-yellow': '#eab308',
        'stream-a': '#22c55e',
        'stream-b': '#3b82f6',
      },
    },
  },
  plugins: [],
}
