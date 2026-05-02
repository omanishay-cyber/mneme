---
name: fireworks-charts
version: 1.0.0
author: Anish Trivedi & Kruti Trivedi
description: amCharts 5 premium chart integration for Electron + React projects. Gauge, Treemap, Sankey, Funnel, Bar Race, Waterfall, Pie/Donut, Radar, Timeline, Heatmap, Wordcloud. React wrapper patterns with useLayoutEffect + refs. Glassmorphism theme integration, dark/light mode, CSS variable bridging.
triggers:
  - gauge
  - treemap
  - sankey
  - funnel
  - bar race
  - waterfall
  - amcharts
  - advanced chart
  - premium chart
  - fireworks-charts
  - new chart type
  - chart upgrade
tags:
  - charts
  - amcharts
  - react
  - electron
  - visualization
  - dataviz
  - premium-ui
  - glassmorphism
---

# Fireworks Charts — amCharts 5 Premium Integration

> amCharts 5 integration patterns for Electron + React + TypeScript projects.

## Installation

```bash
npm install @amcharts/amcharts5
```

No additional packages needed. amCharts 5 includes ALL chart types in one package:
- `@amcharts/amcharts5` — core + percent charts (pie, donut, funnel)
- `@amcharts/amcharts5/xy` — bar, line, area, candlestick, waterfall
- `@amcharts/amcharts5/radar` — gauge, radar, polar
- `@amcharts/amcharts5/flow` — sankey, chord, arc diagrams
- `@amcharts/amcharts5/hierarchy` — treemap, sunburst, force-directed, pack
- `@amcharts/amcharts5/percent` — pie, donut, funnel, pyramid
- `@amcharts/amcharts5/stock` — stock charts, indicators
- `@amcharts/amcharts5/map` — geo maps, choropleth
- `@amcharts/amcharts5/themes/Animated` — entrance animations
- `@amcharts/amcharts5/themes/Dark` — dark mode theme

## License Setup

This is a commercially licensed copy. Add license key in your app's entry point:

```tsx
import * as am5 from '@amcharts/amcharts5';
am5.addLicense('AM5C-XXXX-XXXX-XXXX-XXXX'); // Your license key
```

## Architecture: amCharts vs Recharts

amCharts 5 is **imperative** (Canvas-based), Recharts is **declarative** (SVG + JSX).
They coexist fine — use each where it's strongest:

| Use amCharts For | Use Recharts For |
|---|---|
| Gauge / speedometer | Simple bar/line/area charts |
| Treemap with drill-down | Sparklines in KPI cards |
| Sankey flow diagrams | Charts < 20 data points |
| Funnel / pyramid | Charts with custom SVG effects |
| Bar chart race (animated sorting) | Charts already built with ChartEffects.tsx |
| Heatmaps, wordclouds | Basic pie/donut |
| 365+ data point charts (GPU accel) | |

---

## React Integration Pattern (MANDATORY)

Every amCharts component in React MUST follow this pattern:

```tsx
import { useLayoutEffect, useRef } from 'react';
import * as am5 from '@amcharts/amcharts5';
import am5themes_Animated from '@amcharts/amcharts5/themes/Animated';

interface ChartProps {
  data: SomeDataType[];
  id: string; // unique DOM id — REQUIRED
}

export const MyChart: React.FC<ChartProps> = ({ data, id }) => {
  const rootRef = useRef<am5.Root | null>(null);

  // Initialize chart ONCE
  useLayoutEffect(() => {
    const root = am5.Root.new(id);
    root.setThemes([am5themes_Animated.new(root)]);

    // ... build chart here ...

    rootRef.current = root;
    return () => { root.dispose(); }; // CRITICAL: cleanup on unmount
  }, [id]);

  // Update data when props change
  useLayoutEffect(() => {
    if (rootRef.current) {
      // Update series data
    }
  }, [data]);

  return (
    <div id={id} style={{ width: '100%', height: '100%', minHeight: 200 }} />
  );
};
```

**Rules:**
- `useLayoutEffect` NOT `useEffect` — chart must render before paint
- Always call `root.dispose()` in cleanup — prevents memory leaks
- Each chart needs a **unique DOM id** — never reuse IDs
- Store root in `useRef` for cross-effect access
- Separate initialization from data updates

---

## project Theme Integration

### CSS Variable Bridge

amCharts uses its own color system. Bridge project's CSS variables:

```tsx
function getProjectColor(varName: string): am5.Color {
  const style = getComputedStyle(document.documentElement);
  const hex = style.getPropertyValue(varName).trim();
  return am5.color(hex);
}

// Usage in chart setup:
series.set('fill', getProjectColor('--color-primary'));
series.set('stroke', getProjectColor('--color-primary'));
```

### Dark/Light Mode Switching

```tsx
import { useThemeStore } from '../store/useThemeStore';
import am5themes_Dark from '@amcharts/amcharts5/themes/Dark';
import am5themes_Animated from '@amcharts/amcharts5/themes/Animated';

// Inside useLayoutEffect:
const mode = useThemeStore.getState().mode;
const themes = [am5themes_Animated.new(root)];
if (mode === 'dark') {
  themes.push(am5themes_Dark.new(root));
}
root.setThemes(themes);
```

To react to theme changes, dispose and recreate:

```tsx
const mode = useThemeStore(s => s.mode);

useLayoutEffect(() => {
  if (rootRef.current) rootRef.current.dispose();
  const root = am5.Root.new(id);
  // ... rebuild with correct theme
  rootRef.current = root;
  return () => root.dispose();
}, [mode, id]); // Re-init on theme change
```

### Glass Container Pattern

Wrap amCharts in the project glass card:

```tsx
<div className="backdrop-blur-xl bg-white/10 dark:bg-black/20 border border-white/20 dark:border-white/10 shadow-xl rounded-xl p-4">
  <h3 className="text-sm font-bold mb-3" style={{ color: 'var(--color-text)' }}>
    Chart Title
  </h3>
  <div id={chartId} style={{ width: '100%', height: chartHeight }} />
</div>
```

---

## Chart Recipes

### 1. Gauge Chart (KPI Speedometer)

```tsx
import * as am5 from '@amcharts/amcharts5';
import * as am5xy from '@amcharts/amcharts5/xy';
import * as am5radar from '@amcharts/amcharts5/radar';
import am5themes_Animated from '@amcharts/amcharts5/themes/Animated';

// Inside useLayoutEffect:
const root = am5.Root.new(id);
root.setThemes([am5themes_Animated.new(root)]);

const chart = root.container.children.push(
  am5radar.RadarChart.new(root, {
    panX: false,
    panY: false,
    startAngle: 180,  // half-circle gauge
    endAngle: 360,
  })
);

// Axis
const axisRenderer = am5radar.AxisRendererCircular.new(root, {
  innerRadius: -40,
});
axisRenderer.grid.template.setAll({
  stroke: root.interfaceColors.get('background'),
  visible: true,
  strokeOpacity: 0.8,
});

const xAxis = chart.xAxes.push(
  am5xy.ValueAxis.new(root, {
    maxDeviation: 0,
    min: 0,
    max: 100,
    strictMinMax: true,
    renderer: axisRenderer,
  })
);

// Color bands (green/yellow/red zones)
const bands = [
  { title: 'Low', color: '#ee1f25', low: 0, high: 30 },
  { title: 'Medium', color: '#fdae19', low: 30, high: 60 },
  { title: 'Good', color: '#54b947', low: 60, high: 80 },
  { title: 'Excellent', color: '#0f9747', low: 80, high: 100 },
];

bands.forEach((band) => {
  const range = xAxis.createAxisRange(
    xAxis.makeDataItem({ above: true, value: band.low, endValue: band.high })
  );
  range.get('axisFill').setAll({
    visible: true,
    fill: am5.color(band.color),
    fillOpacity: 0.8,
  });
  range.get('label').setAll({
    text: band.title,
    inside: true,
    radius: 15,
    fontSize: '0.9em',
    fill: root.interfaceColors.get('background'),
  });
});

// Clock hand (needle)
const axisDataItem = xAxis.makeDataItem({});
axisDataItem.set('value', 0);

const clockHand = am5radar.ClockHand.new(root, {
  pinRadius: am5.percent(20),
  radius: am5.percent(100),
  bottomWidth: 40,
});

axisDataItem.set(
  'bullet',
  am5xy.AxisBullet.new(root, { sprite: clockHand })
);
xAxis.createAxisRange(axisDataItem);

// Animate to target value
axisDataItem.animate({
  key: 'value',
  to: targetValue, // e.g. 72 for 72% margin
  duration: 1000,
  easing: am5.ease.out(am5.ease.cubic),
});

chart.appear(1000, 100);
```

**Example use cases:** Gross Margin %, Sales Target %, Expense Ratio

---

### 2. Treemap (Category Revenue)

```tsx
import * as am5 from '@amcharts/amcharts5';
import * as am5hierarchy from '@amcharts/amcharts5/hierarchy';
import am5themes_Animated from '@amcharts/amcharts5/themes/Animated';

const root = am5.Root.new(id);
root.setThemes([am5themes_Animated.new(root)]);

const container = root.container.children.push(
  am5.Container.new(root, {
    width: am5.percent(100),
    height: am5.percent(100),
    layout: root.verticalLayout,
  })
);

const series = container.children.push(
  am5hierarchy.Treemap.new(root, {
    singleBranchOnly: false,
    downDepth: 1,
    upDepth: 1,
    initialDepth: 1,
    valueField: 'value',
    categoryField: 'name',
    childDataField: 'children',
  })
);

// project business data structure:
const data = {
  name: 'Revenue',
  children: [
    {
      name: 'Spirits',
      children: [
        { name: 'Whiskey', value: 45000 },
        { name: 'Vodka', value: 32000 },
        { name: 'Rum', value: 18000 },
        { name: 'Tequila', value: 15000 },
        { name: 'Gin', value: 12000 },
      ],
    },
    {
      name: 'Wine',
      children: [
        { name: 'Red Wine', value: 28000 },
        { name: 'White Wine', value: 15000 },
        { name: 'Rose', value: 8000 },
      ],
    },
    {
      name: 'Beer',
      children: [
        { name: 'Domestic', value: 22000 },
        { name: 'Craft', value: 12000 },
        { name: 'Import', value: 9000 },
      ],
    },
    {
      name: 'Other',
      children: [
        { name: 'Mixers', value: 5000 },
        { name: 'Snacks', value: 3000 },
        { name: 'Tobacco', value: 7000 },
      ],
    },
  ],
};

series.data.setAll([data]);
series.set('selectedDataItem', series.dataItems[0]);

// Breadcrumb navigation for drill-down
container.children.moveValue(
  am5hierarchy.BreadcrumbBar.new(root, { series }),
  0
);

series.appear(1000, 100);
```

**Example use cases:** Product category revenue, Vendor spend breakdown, COGS by supplier

---

### 3. Sankey Flow Diagram (Money Flow)

```tsx
import * as am5 from '@amcharts/amcharts5';
import * as am5flow from '@amcharts/amcharts5/flow';
import am5themes_Animated from '@amcharts/amcharts5/themes/Animated';

const root = am5.Root.new(id);
root.setThemes([am5themes_Animated.new(root)]);

const series = root.container.children.push(
  am5flow.Sankey.new(root, {
    sourceIdField: 'from',
    targetIdField: 'to',
    valueField: 'value',
    paddingRight: 50,
  })
);

series.nodes.get('colors').set('step', 2);

// project P&L flow:
series.data.setAll([
  { from: 'Gross Revenue', to: 'COGS', value: 180000 },
  { from: 'Gross Revenue', to: 'Gross Profit', value: 120000 },
  { from: 'Gross Profit', to: 'Rent', value: 25000 },
  { from: 'Gross Profit', to: 'Utilities', value: 10000 },
  { from: 'Gross Profit', to: 'Payroll', value: 30000 },
  { from: 'Gross Profit', to: 'Insurance', value: 5000 },
  { from: 'Gross Profit', to: 'Net Profit', value: 50000 },
]);

series.appear(1000, 100);
```

**Example use cases:** Revenue-to-profit flow, expense allocation, payment method breakdown

---

### 4. Funnel Chart (Sales Compression)

```tsx
import * as am5 from '@amcharts/amcharts5';
import * as am5percent from '@amcharts/amcharts5/percent';
import am5themes_Animated from '@amcharts/amcharts5/themes/Animated';

const root = am5.Root.new(id);
root.setThemes([am5themes_Animated.new(root)]);

const chart = root.container.children.push(
  am5percent.SlicedChart.new(root, { layout: root.verticalLayout })
);

const series = chart.series.push(
  am5percent.FunnelSeries.new(root, {
    alignLabels: false,
    orientation: 'vertical',
    valueField: 'value',
    categoryField: 'category',
  })
);

// project margin compression funnel:
series.data.setAll([
  { value: 300000, category: 'Gross Sales' },
  { value: 285000, category: 'Net Sales' },
  { value: 120000, category: 'Gross Profit' },
  { value: 75000, category: 'Operating Profit' },
  { value: 58000, category: 'Net Profit' },
]);

series.appear();

const legend = chart.children.push(
  am5.Legend.new(root, {
    centerX: am5.p50,
    x: am5.p50,
    marginTop: 15,
    marginBottom: 15,
  })
);
legend.data.setAll(series.dataItems);

chart.appear(1000, 100);
```

**Example use cases:** Revenue-to-profit funnel, customer conversion pipeline

---

### 5. Bar Chart Race (Animated Sorting)

```tsx
import * as am5 from '@amcharts/amcharts5';
import * as am5xy from '@amcharts/amcharts5/xy';
import am5themes_Animated from '@amcharts/amcharts5/themes/Animated';

const root = am5.Root.new(id);
root.setThemes([am5themes_Animated.new(root)]);

const chart = root.container.children.push(
  am5xy.XYChart.new(root, {
    panX: true,
    panY: true,
    wheelX: 'none',
    wheelY: 'none',
  })
);

chart.zoomOutButton.set('forceHidden', true);

// Category axis (products)
const xRenderer = am5xy.AxisRendererX.new(root, { minGridDistance: 30 });
xRenderer.labels.template.setAll({
  rotation: -90,
  centerY: am5.p50,
  centerX: 0,
  paddingRight: 15,
});
xRenderer.grid.template.set('visible', false);

const xAxis = chart.xAxes.push(
  am5xy.CategoryAxis.new(root, {
    maxDeviation: 0.3,
    categoryField: 'name',
    renderer: xRenderer,
  })
);

const yAxis = chart.yAxes.push(
  am5xy.ValueAxis.new(root, {
    maxDeviation: 0.3,
    min: 0,
    renderer: am5xy.AxisRendererY.new(root, {}),
  })
);

const series = chart.series.push(
  am5xy.ColumnSeries.new(root, {
    name: 'Sales',
    xAxis,
    yAxis,
    valueYField: 'value',
    categoryXField: 'name',
  })
);

series.columns.template.setAll({
  cornerRadiusTL: 5,
  cornerRadiusTR: 5,
});

// Each bar gets a different color
series.columns.template.adapters.add('fill', (fill, target) => {
  return chart.get('colors').getIndex(series.columns.indexOf(target));
});

// Sort function — call this after data updates
function sortCategoryAxis() {
  series.dataItems.sort((x, y) => y.get('valueY') - x.get('valueY'));

  am5.array.each(xAxis.dataItems, (dataItem) => {
    const seriesItem = series.dataItems.find(
      (s) => s.get('categoryX') === dataItem.get('category')
    );
    if (seriesItem) {
      const index = series.dataItems.indexOf(seriesItem);
      const delta = (index - dataItem.get('index', 0)) / series.dataItems.length;
      dataItem.set('index', index);
      dataItem.set('deltaPosition', -delta);
      dataItem.animate({
        key: 'deltaPosition',
        to: 0,
        duration: 1000,
        easing: am5.ease.out(am5.ease.cubic),
      });
    }
  });

  xAxis.dataItems.sort((x, y) => x.get('index') - y.get('index'));
}

// Set initial data and start race
xAxis.data.setAll(initialData);
series.data.setAll(initialData);

// Update data periodically (e.g., cycle through months)
// Call sortCategoryAxis() after each data update

series.appear(1000);
chart.appear(1000, 100);
```

**Example use cases:** Top products by month (race through the year), vendor ranking changes

---

### 6. Animated Pie/Donut

```tsx
import * as am5 from '@amcharts/amcharts5';
import * as am5percent from '@amcharts/amcharts5/percent';
import am5themes_Animated from '@amcharts/amcharts5/themes/Animated';

const root = am5.Root.new(id);
root.setThemes([am5themes_Animated.new(root)]);

const chart = root.container.children.push(
  am5percent.PieChart.new(root, {
    layout: root.verticalLayout,
    innerRadius: am5.percent(50), // 0 for pie, 50+ for donut
  })
);

const series = chart.series.push(
  am5percent.PieSeries.new(root, {
    valueField: 'value',
    categoryField: 'category',
    alignLabels: false,
  })
);

series.labels.template.setAll({
  textType: 'circular',
  centerX: 0,
  centerY: 0,
});

series.data.setAll([
  { value: 45000, category: 'Spirits' },
  { value: 28000, category: 'Wine' },
  { value: 22000, category: 'Beer' },
  { value: 15000, category: 'Other' },
]);

// Legend
const legend = chart.children.push(
  am5.Legend.new(root, {
    centerX: am5.percent(50),
    x: am5.percent(50),
    marginTop: 15,
  })
);
legend.data.setAll(series.dataItems);

series.appear(1000, 100);
```

---

### 7. Waterfall Chart (P&L Walk)

Built using XY chart with stacked columns and invisible base segments:

```tsx
import * as am5 from '@amcharts/amcharts5';
import * as am5xy from '@amcharts/amcharts5/xy';
import am5themes_Animated from '@amcharts/amcharts5/themes/Animated';

const root = am5.Root.new(id);
root.setThemes([am5themes_Animated.new(root)]);

const chart = root.container.children.push(
  am5xy.XYChart.new(root, { panX: false, panY: false })
);

const xAxis = chart.xAxes.push(
  am5xy.CategoryAxis.new(root, {
    categoryField: 'category',
    renderer: am5xy.AxisRendererX.new(root, { minGridDistance: 30 }),
  })
);

const yAxis = chart.yAxes.push(
  am5xy.ValueAxis.new(root, {
    renderer: am5xy.AxisRendererY.new(root, {}),
  })
);

// Invisible base series
const baseSeries = chart.series.push(
  am5xy.ColumnSeries.new(root, {
    xAxis,
    yAxis,
    valueYField: 'base',
    categoryXField: 'category',
    stacked: true,
  })
);
baseSeries.columns.template.setAll({
  fillOpacity: 0,
  strokeOpacity: 0,
});

// Visible value series
const valueSeries = chart.series.push(
  am5xy.ColumnSeries.new(root, {
    xAxis,
    yAxis,
    valueYField: 'value',
    categoryXField: 'category',
    stacked: true,
  })
);

// Color positive green, negative red
valueSeries.columns.template.adapters.add('fill', (fill, target) => {
  const val = target.dataItem?.get('valueY') ?? 0;
  return val >= 0 ? am5.color(0x54b947) : am5.color(0xee1f25);
});

// project P&L waterfall data:
const data = [
  { category: 'Revenue', base: 0, value: 300000 },
  { category: 'COGS', base: 120000, value: -180000 },
  { category: 'Gross Profit', base: 0, value: 120000 },
  { category: 'Rent', base: 95000, value: -25000 },
  { category: 'Utilities', base: 85000, value: -10000 },
  { category: 'Payroll', base: 55000, value: -30000 },
  { category: 'Net Profit', base: 0, value: 55000 },
];

xAxis.data.setAll(data);
baseSeries.data.setAll(data);
valueSeries.data.setAll(data);

chart.appear(1000, 100);
```

---

## Modules Reference

| Import | Chart Types |
|--------|-------------|
| `@amcharts/amcharts5` | Core, Root, Theme, Colors, Labels |
| `@amcharts/amcharts5/xy` | Bar, Line, Area, Candlestick, ValueAxis, CategoryAxis, DateAxis |
| `@amcharts/amcharts5/radar` | RadarChart, Gauge (ClockHand), Polar |
| `@amcharts/amcharts5/percent` | PieChart, PieSeries, SlicedChart, FunnelSeries, PyramidSeries |
| `@amcharts/amcharts5/flow` | Sankey, Chord, ArcDiagram |
| `@amcharts/amcharts5/hierarchy` | Treemap, Sunburst, ForceDirected, Pack, Tree, Partition |
| `@amcharts/amcharts5/stock` | StockChart, indicators, drawing tools |
| `@amcharts/amcharts5/map` | MapChart, GeoJSON, Choropleth |
| `@amcharts/amcharts5/themes/Animated` | Entrance animations |
| `@amcharts/amcharts5/themes/Dark` | Dark mode colors |

## Animation API

```tsx
// Animate any property
dataItem.animate({
  key: 'value',
  to: 75,
  duration: 800,
  easing: am5.ease.out(am5.ease.cubic),
});

// Easing functions
am5.ease.linear
am5.ease.out(am5.ease.cubic)    // smooth deceleration — most common
am5.ease.out(am5.ease.elastic)  // bounce effect
am5.ease.inOut(am5.ease.cubic)  // smooth both ways

// Appear animation (entrance)
chart.appear(1000, 100);  // duration, delay
series.appear(1000, 100);
```

## Key Differences from Recharts

| Recharts | amCharts 5 |
|----------|------------|
| `<BarChart data={data}>` | `am5xy.XYChart.new(root, {})` |
| `<Bar dataKey="value" />` | `am5xy.ColumnSeries.new(root, { valueYField: 'value' })` |
| `<Tooltip content={<Custom />} />` | `series.set('tooltip', am5.Tooltip.new(root, {}))` |
| `<ResponsiveContainer>` | Container auto-fills parent div |
| Re-render on state change | Manual `series.data.setAll(newData)` |
| SVG output | Canvas output (GPU accelerated) |
| ~150KB | ~500KB (but tree-shakeable) |

## Coexistence with Recharts

Both libraries work side by side in the same project:
- Recharts renders to SVG inside React's virtual DOM
- amCharts renders to Canvas in a raw DOM div
- No conflicts, no shared globals
- Use Recharts for charts with existing `ChartEffects.tsx` premium styling
- Use amCharts for new chart types Recharts doesn't support

## File Organization Pattern

```
src/
  components/
    charts/
      amcharts/
        GaugeChart.tsx
        TreemapChart.tsx
        SankeyFlow.tsx
        FunnelChart.tsx
        BarRaceChart.tsx
        WaterfallChart.tsx
        AmDonutChart.tsx
        useAmChart.ts          // shared hook for root lifecycle
        amChartsTheme.ts       // CSS variable bridge + project colors
      ChartEffects.tsx          // existing Recharts effects (keep)
  hooks/
    useChartHeight.ts           // existing (works with both)
```

### Shared Hook: `useAmChart.ts`

```tsx
import { useLayoutEffect, useRef } from 'react';
import * as am5 from '@amcharts/amcharts5';
import am5themes_Animated from '@amcharts/amcharts5/themes/Animated';
import am5themes_Dark from '@amcharts/amcharts5/themes/Dark';

export function useAmChart(
  id: string,
  isDark: boolean,
  setup: (root: am5.Root) => void | (() => void)
) {
  const rootRef = useRef<am5.Root | null>(null);

  useLayoutEffect(() => {
    const root = am5.Root.new(id);

    const themes: am5.Theme[] = [am5themes_Animated.new(root)];
    if (isDark) themes.push(am5themes_Dark.new(root));
    root.setThemes(themes);

    const cleanup = setup(root);
    rootRef.current = root;

    return () => {
      if (cleanup) cleanup();
      root.dispose();
    };
  }, [id, isDark]);

  return rootRef;
}
```

Usage:

```tsx
const mode = useThemeStore(s => s.mode);

useAmChart('gauge-1', mode === 'dark', (root) => {
  const chart = root.container.children.push(
    am5radar.RadarChart.new(root, { startAngle: 180, endAngle: 360 })
  );
  // ... build gauge
  chart.appear(1000, 100);
});
```

## Source Code Reference

amCharts 5 ships full source on npm. After `npm install @amcharts/amcharts5`,
key directories under `node_modules/@amcharts/amcharts5/`:

- `.internal/charts/` — all chart type implementations
- `.internal/core/` — rendering engine, Root, Entity system
- `.internal/themes/` — AnimatedTheme, DarkTheme, etc.

Upstream examples and recipes: <https://www.amcharts.com/demos/>.
