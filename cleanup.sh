#!/bin/bash
# OrbStack / Docker Resource Cleanup Script
# This script automates the cleanup of Docker containers, images, volumes, and networks

set -e  # Exit on any error

echo "🧹 Starting Docker/OrbStack cleanup..."

echo "📊 Current disk usage:"
docker system df

echo ""
echo "📋 Current containers:"
docker ps -a

echo ""
echo "🖼️  Current images:"
docker images -a

echo ""
echo "💾 Current volumes:"
docker volume ls

echo ""
echo "🌐 Current networks:"
docker network ls

echo ""
echo "🗑️  Removing stopped containers..."
docker container prune -f

echo ""
echo "🗑️  Removing dangling images..."
docker image prune -f

echo ""
echo "🗑️  Removing unused volumes (WARNING: This will remove all unused volumes)..."
read -p "Continue with volume cleanup? [y/N]: " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    docker volume prune -f
else
    echo "Skipping volume cleanup."
fi

echo ""
echo "🗑️  Removing unused networks..."
docker network prune -f

echo ""
echo "📊 Final disk usage:"
docker system df

echo ""
echo "✅ Cleanup complete!"
echo ""
echo "💡 For more aggressive cleanup (removes all unused images, not just dangling ones):"
echo "   docker system prune -a"
echo ""
echo "📖 For OrbStack documentation, visit: https://docs.orbstack.dev/"
